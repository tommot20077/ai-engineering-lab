//! `gateway.toml` 的形狀與啟動檢查(design-part2.md §16、D-024)。
//!
//! 兩層型別:
//! - `RawConfig`:檔案裡寫什麼就是什麼(serde 直接讀進來)。
//! - `Settings`:檢查過、補上預設值、路徑解析好的結果。之後的程式只看得到這一層。
//!
//! Java 對照:像是 `@ConfigurationProperties` 讀進來的 POJO,加上 `@Validated` 的檢查;
//! 差別是這裡檢查完會換成另一個型別,「沒檢查過的設定」在型別上就拿不到後面去用。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use config_loader::ConfigFile;
use llm::{DEFAULT_BASE_URL, RetryPolicy};
use serde::Deserialize;
use url::{Host, Url};

pub const DEFAULT_BIND: &str = "127.0.0.1:8787";
pub const DEFAULT_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT: usize = 16;

/// 設定檔的原始內容。
// `deny_unknown_fields`:不認得的欄位直接報錯。打成 `max_atempts` 不能被默默忽略(D-024)。
// 每一個區段都要各自加,serde 不會往下繼承。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    // `#[serde(default)]`:整個區段省略時,用該型別的 `Default`。
    #[serde(default)]
    pub server: ServerSection,
    /// 必填:沒有 `[aliases]` 就是解析錯誤。
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub anthropic: AnthropicSection,
    #[serde(default)]
    pub record: RecordSection,
    #[serde(default)]
    pub retry: RetrySection,
    #[serde(default)]
    pub limits: LimitsSection,
}

// 區段上的 `default`:區段在,但只寫了其中幾個欄位時,其他欄位用 `Default` 的值。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    pub bind: String,
    pub allow_remote: bool,
    pub allowed_hosts: Vec<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.into(),
            allow_remote: false,
            allowed_hosts: vec![],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AnthropicSection {
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RecordSection {
    pub enabled: bool,
    pub dir: Option<String>,
}

impl Default for RecordSection {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetrySection {
    pub max_attempts: u32,
    pub first_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetrySection {
    // 預設值只寫在一個地方:`RetryPolicy::default()`(D-022),這裡換算成毫秒。
    fn default() -> Self {
        let policy = RetryPolicy::default();
        Self {
            max_attempts: policy.max_attempts,
            first_delay_ms: millis(policy.first_delay),
            max_delay_ms: millis(policy.max_delay),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    pub max_body_bytes: usize,
    pub max_concurrent: usize,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }
}

/// 檢查過的設定。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub bind: SocketAddr,
    /// 請求的 `Host` header 只能是這些(擋 DNS rebinding,D-024)。
    pub allowed_hosts: Vec<String>,
    pub aliases: BTreeMap<String, String>,
    pub anthropic_base_url: String,
    /// `None` = 不記錄。
    pub record_dir: Option<PathBuf>,
    pub retry: RetryPolicy,
    pub max_body_bytes: usize,
    pub max_concurrent: usize,
}

/// 設定有問題。一次列出**全部**的問題,不要讓人改一個、重跑、再看下一個。
#[derive(Debug, thiserror::Error)]
#[error("invalid gateway config:\n{}", bullet_list(.0))]
pub struct InvalidConfig(pub Vec<String>);

fn bullet_list(problems: &[String]) -> String {
    problems
        .iter()
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Settings {
    /// 檢查設定、補上預設值、解析路徑。
    ///
    /// `default_records`:沒寫 `record.dir` 時的預設目錄。用函式傳進來,測試才不必碰真的家目錄。
    pub fn from_config(
        file: &ConfigFile<RawConfig>,
        default_records: impl FnOnce() -> config_loader::Result<PathBuf>,
    ) -> Result<Self, InvalidConfig> {
        let raw = &file.value;
        // 每個檢查都把問題丟進同一個清單,不提早 return:最後一次全部列出來。
        let mut problems = Vec::new();

        let bind = parse_bind(&raw.server.bind, &mut problems);
        let allowed_hosts = bind
            .map(|bind| allowed_hosts(bind, &raw.server, &mut problems))
            .unwrap_or_default();
        check_aliases(&raw.aliases, &mut problems);
        let base_url = check_base_url(raw.anthropic.base_url.as_deref(), &mut problems);
        let retry = check_retry(&raw.retry, &mut problems);
        check_limits(&raw.limits, &mut problems);

        let record_dir = match (&raw.record.enabled, &raw.record.dir) {
            (false, _) => Some(None),
            // 相對路徑以設定檔所在目錄為準(config-loader,D-025)。
            (true, Some(dir)) => file
                .resolve(dir)
                .map(Some)
                .map_err(|e| problems.push(format!("record.dir: {e}")))
                .ok(),
            (true, None) => default_records()
                .map(Some)
                .map_err(|e| problems.push(format!("default record directory: {e}")))
                .ok(),
        };

        // 只有「沒有任何問題」時,上面那些 Option 才一定都是 Some。
        // 把兩個條件寫在同一個 match 裡,就不需要 unwrap。
        match (bind, base_url, record_dir) {
            (Some(bind), Some(anthropic_base_url), Some(record_dir)) if problems.is_empty() => {
                Ok(Self {
                    bind,
                    allowed_hosts,
                    aliases: raw.aliases.clone(),
                    anthropic_base_url,
                    record_dir,
                    retry,
                    max_body_bytes: raw.limits.max_body_bytes,
                    max_concurrent: raw.limits.max_concurrent,
                })
            }
            _ => Err(InvalidConfig(problems)),
        }
    }
}

fn parse_bind(bind: &str, problems: &mut Vec<String>) -> Option<SocketAddr> {
    // `SocketAddr` 只接受 IP:port,不做 DNS 查詢;`localhost:8787` 會被拒絕,這是刻意的:
    // 聽在哪個位址要明確,不能取決於 DNS 回答什麼。
    bind.parse()
        .map_err(|_| {
            problems.push(format!(
                "server.bind `{bind}` must be an IP address and port, like 127.0.0.1:8787"
            ));
        })
        .ok()
}

/// 算出允許的 `Host` header。本機 bind 自動產生;對外 bind 要明確列出(D-024)。
fn allowed_hosts(
    bind: SocketAddr,
    server: &ServerSection,
    problems: &mut Vec<String>,
) -> Vec<String> {
    let port = bind.port();
    if bind.ip().is_loopback() {
        if server.allow_remote || !server.allowed_hosts.is_empty() {
            problems.push(
                "server.allow_remote and server.allowed_hosts only apply when server.bind is not a loopback address"
                    .into(),
            );
        }
        return vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
    }
    // 對外開放要「寫兩次」才算數:防止為了測試隨手改成 0.0.0.0、之後忘了改回來。
    if !server.allow_remote {
        problems.push(format!(
            "server.bind {bind} is reachable from other machines; set server.allow_remote = true to confirm"
        ));
    } else if server.allowed_hosts.is_empty() {
        problems.push(
            "server.allow_remote needs server.allowed_hosts (the host:port clients will connect to)".into(),
        );
    }
    server.allowed_hosts.clone()
}

fn check_aliases(aliases: &BTreeMap<String, String>, problems: &mut Vec<String>) {
    if aliases.is_empty() {
        problems.push("[aliases] must define at least one alias".into());
    }
    for (alias, model) in aliases {
        if alias.trim().is_empty() || model.trim().is_empty() {
            problems.push(format!(
                "[aliases] `{alias}` must map a non-empty name to a non-empty model id"
            ));
        }
    }
}

fn check_base_url(raw: Option<&str>, problems: &mut Vec<String>) -> Option<String> {
    let raw = raw.unwrap_or(DEFAULT_BASE_URL);
    let Ok(url) = Url::parse(raw) else {
        problems.push(format!("anthropic.base_url `{raw}` is not a valid URL"));
        return None;
    };
    // API key 放在 header 裡送出:明文 http 只准連到本機(給測試用的假 server)。
    // `Host` 是 enum:網域、IPv4、IPv6 分開表示,不必自己拆字串。
    let local = match url.host() {
        Some(Host::Domain(host)) => host == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Some(raw.to_owned()),
        "http" if local => Some(raw.to_owned()),
        _ => {
            problems.push(format!(
                "anthropic.base_url `{raw}` must use https (plain http is only allowed for localhost)"
            ));
            None
        }
    }
}

fn check_retry(retry: &RetrySection, problems: &mut Vec<String>) -> RetryPolicy {
    if retry.max_attempts == 0 {
        problems.push("retry.max_attempts must be at least 1 (1 = no retries)".into());
    }
    if retry.max_delay_ms == 0 {
        problems.push("retry.max_delay_ms must be greater than 0".into());
    }
    if retry.first_delay_ms > retry.max_delay_ms {
        problems.push(format!(
            "retry.first_delay_ms ({}) must not exceed retry.max_delay_ms ({})",
            retry.first_delay_ms, retry.max_delay_ms
        ));
    }
    RetryPolicy {
        max_attempts: retry.max_attempts,
        first_delay: Duration::from_millis(retry.first_delay_ms),
        max_delay: Duration::from_millis(retry.max_delay_ms),
    }
}

fn check_limits(limits: &LimitsSection, problems: &mut Vec<String>) {
    if limits.max_body_bytes == 0 {
        problems.push("limits.max_body_bytes must be greater than 0".into());
    }
    if limits.max_concurrent == 0 {
        problems.push("limits.max_concurrent must be at least 1".into());
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "[aliases]\ncheap = \"claude-haiku-5-5\"\n";

    fn file(text: &str) -> ConfigFile<RawConfig> {
        ConfigFile {
            value: toml::from_str(text).unwrap(),
            dir: PathBuf::from("/etc/gw"),
        }
    }

    fn settings(text: &str) -> Result<Settings, InvalidConfig> {
        Settings::from_config(&file(text), || Ok(PathBuf::from("/data/gateway/records")))
    }

    /// 斷言設定被拒絕,而且問題清單裡有一條包含 `needle`。
    fn rejected(text: &str, needle: &str) {
        let err = settings(text).expect_err("應該被拒絕");
        assert!(
            err.0.iter().any(|p| p.contains(needle)),
            "找不到「{needle}」:{err}"
        );
    }

    #[test]
    fn the_example_file_in_the_repo_is_valid() {
        // 範例檔就是文件:它壞掉的話,第一個照著做的人就啟動不了。
        let s = settings(include_str!("../../../gateway.example.toml")).unwrap();
        assert_eq!(s.aliases["cheap"], "claude-haiku-5-5");
        assert_eq!(s.aliases["default"], "claude-sonnet-5-5");
    }

    #[test]
    fn the_example_file_writes_down_the_real_defaults() {
        // 範例檔把預設值寫成數字(10485760、1000…),程式的預設值是常數算出來的。
        // 兩邊要一致:範例檔不能騙人,常數算錯(例如 `10 * 1024 * 1024` 寫成 `10 + 1024 * 1024`)也要被抓到。
        // (cargo-mutants 找到的缺口:原本的預設值測試拿同一個常數比對,常數本身錯了看不出來。)
        let example = settings(include_str!("../../../gateway.example.toml")).unwrap();
        assert_eq!(example.bind, DEFAULT_BIND.parse().unwrap());
        assert_eq!(example.retry, RetryPolicy::default());
        assert_eq!(example.max_body_bytes, 10 * 1024 * 1024);
        assert_eq!(example.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
        assert_eq!(example.max_concurrent, DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn a_minimal_file_gets_safe_defaults() {
        let s = settings(MINIMAL).unwrap();
        assert_eq!(
            s,
            Settings {
                bind: "127.0.0.1:8787".parse().unwrap(),
                allowed_hosts: vec![
                    "127.0.0.1:8787".into(),
                    "localhost:8787".into(),
                    "[::1]:8787".into()
                ],
                aliases: BTreeMap::from([("cheap".into(), "claude-haiku-5-5".into())]),
                anthropic_base_url: DEFAULT_BASE_URL.into(),
                record_dir: Some(PathBuf::from("/data/gateway/records")),
                retry: RetryPolicy::default(),
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                max_concurrent: DEFAULT_MAX_CONCURRENT,
            }
        );
    }

    #[test]
    fn typos_are_rejected_in_every_section() {
        for text in [
            "[aliases]\ncheap = \"x\"\n[retry]\nmax_atempts = 5\n",
            "[aliases]\ncheap = \"x\"\n[retyr]\nmax_attempts = 5\n",
            "[aliases]\ncheap = \"x\"\n[server]\nbnid = \"127.0.0.1:1\"\n",
        ] {
            let err = toml::from_str::<RawConfig>(text).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
    }

    #[test]
    fn aliases_must_not_be_empty() {
        rejected("[aliases]\n", "at least one alias");
        rejected("[aliases]\ncheap = \"\"\n", "cheap");
    }

    #[test]
    fn a_bind_reachable_from_other_machines_needs_two_confirmations() {
        let remote = "[aliases]\ncheap = \"x\"\n[server]\nbind = \"0.0.0.0:8787\"\n";
        // 比對要夠具體:下一條檢查的訊息裡也有「allow_remote」這個字,只比對它會用錯的理由通過。
        rejected(remote, "reachable from other machines");

        let no_hosts =
            "[aliases]\ncheap = \"x\"\n[server]\nbind = \"0.0.0.0:8787\"\nallow_remote = true\n";
        rejected(no_hosts, "allowed_hosts");

        let ok = "[aliases]\ncheap = \"x\"\n[server]\nbind = \"0.0.0.0:8787\"\nallow_remote = true\nallowed_hosts = [\"gw.lan:8787\"]\n";
        assert_eq!(settings(ok).unwrap().allowed_hosts, ["gw.lan:8787"]);
    }

    #[test]
    fn remote_options_on_a_local_bind_are_rejected_as_confusing() {
        rejected(
            "[aliases]\ncheap = \"x\"\n[server]\nallow_remote = true\n",
            "only apply",
        );
    }

    #[test]
    fn bind_must_be_an_ip_address_and_port() {
        rejected(
            "[aliases]\ncheap = \"x\"\n[server]\nbind = \"localhost:8787\"\n",
            "server.bind",
        );
    }

    #[test]
    fn retry_and_limit_numbers_must_make_sense() {
        rejected(
            "[aliases]\ncheap = \"x\"\n[retry]\nmax_attempts = 0\n",
            "max_attempts",
        );
        rejected(
            "[aliases]\ncheap = \"x\"\n[retry]\nfirst_delay_ms = 5000\nmax_delay_ms = 1000\n",
            "first_delay_ms",
        );
        rejected(
            "[aliases]\ncheap = \"x\"\n[limits]\nmax_body_bytes = 0\n",
            "max_body_bytes",
        );
        rejected(
            "[aliases]\ncheap = \"x\"\n[limits]\nmax_concurrent = 0\n",
            "max_concurrent",
        );
    }

    #[test]
    fn base_url_must_be_https_except_for_localhost() {
        // API key 會放在 header 裡送出:不准走明文 http 到別的機器。
        rejected(
            "[aliases]\ncheap = \"x\"\n[anthropic]\nbase_url = \"http://api.example.com\"\n",
            "https",
        );
        rejected(
            "[aliases]\ncheap = \"x\"\n[anthropic]\nbase_url = \"not a url\"\n",
            "base_url",
        );
        // 名字像 localhost 不算:只有真的是 localhost 才行。
        rejected(
            "[aliases]\ncheap = \"x\"\n[anthropic]\nbase_url = \"http://localhost.evil.com\"\n",
            "https",
        );
        for ok in [
            "https://proxy.example.com",
            "http://127.0.0.1:9000",
            "http://localhost:9000",
            "http://[::1]:9000",
        ] {
            let text = format!("[aliases]\ncheap = \"x\"\n[anthropic]\nbase_url = \"{ok}\"\n");
            assert_eq!(settings(&text).unwrap().anthropic_base_url, ok);
        }
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let err = settings("[aliases]\n[retry]\nmax_attempts = 0\n").unwrap_err();
        assert_eq!(err.0.len(), 2, "{err}");
    }

    #[test]
    fn the_error_message_lists_each_problem_on_its_own_line() {
        // 使用者實際看到的就是這段文字(main 印出來的),格式也要測。
        // (cargo-mutants 找到的缺口:只檢查了問題清單的內容,沒檢查印出來的樣子。)
        let err = settings("[aliases]\n[retry]\nmax_attempts = 0\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid gateway config:\n\
             \x20 - [aliases] must define at least one alias\n\
             \x20 - retry.max_attempts must be at least 1 (1 = no retries)"
        );
    }

    #[test]
    fn first_delay_may_equal_max_delay() {
        // 邊界:相等是合法的(每次都等同樣久)。只有「大於」才是錯。
        // (cargo-mutants 找到的缺口:`>` 改成 `>=` 時沒有測試失敗。)
        let text =
            "[aliases]\ncheap = \"x\"\n[retry]\nfirst_delay_ms = 2000\nmax_delay_ms = 2000\n";
        assert_eq!(
            settings(text).unwrap().retry.first_delay,
            Duration::from_millis(2000)
        );
    }

    #[test]
    fn record_dir_is_resolved_against_the_config_directory() {
        let custom = "[aliases]\ncheap = \"x\"\n[record]\ndir = \"./records\"\n";
        assert_eq!(
            settings(custom).unwrap().record_dir,
            Some(PathBuf::from("/etc/gw/records"))
        );
        let off = "[aliases]\ncheap = \"x\"\n[record]\nenabled = false\n";
        assert_eq!(settings(off).unwrap().record_dir, None);
    }
}
