//! 給 binary 用的共用設定工具(design-part2.md §16、D-025)。
//!
//! gateway、之後的 eval、個人助理都會讀設定檔,規則要一致:
//! - 路徑:只展開開頭的 `~/`;不展開 `$VAR`;**相對路徑以設定檔所在目錄為準**(同 Cargo)。
//! - 讀 TOML:錯誤訊息帶檔名與行號。
//! - 秘密:從環境變數讀,直接包成 `SecretString`。
//!
//! library(例如 `llm`)不依賴這個 crate:library 只從 constructor 拿到值(D-016)。
//!
//! 每個函式都分兩層:外層讀真正的環境變數,內層是值從參數進來的純函式。
//! Rust 2024 把 `std::env::set_var` 改成 `unsafe`(環境變數是整個 process 共用的狀態,
//! 多執行緒同時改會出事),而這個 workspace 禁止 unsafe,所以測試只測內層。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde::de::DeserializeOwned;

/// 所有程式共用的資料目錄名稱。
pub const APP_DIR: &str = "ai-engineering-lab";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file {} not found", .0.display())]
    NotFound(PathBuf),

    #[error("cannot read config file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// TOML 語法錯誤、型別不對、或不認得的欄位。`message` 來自 toml,已經帶行號與欄號。
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },

    #[error("cannot find the home directory: HOME is not set")]
    NoHome,

    #[error("only `~/` is supported at the start of a path, got `{0}`")]
    UnsupportedTilde(String),

    // 秘密相關的錯誤只放「變數名稱」,絕對不放值。
    #[error("environment variable {0} is not set")]
    MissingEnv(String),

    #[error("environment variable {0} is empty")]
    EmptyEnv(String),

    #[error("environment variable {0} is not valid UTF-8")]
    NotUnicodeEnv(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// 讀進來的設定,以及它所在的目錄。設定裡的相對路徑都以 `dir` 為準。
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile<T> {
    pub value: T,
    /// 設定檔所在的目錄(絕對路徑)。
    pub dir: PathBuf,
}

impl<T> ConfigFile<T> {
    /// 解析這份設定裡寫的路徑:相對路徑以設定檔所在目錄為準。
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        resolve_path(raw, &self.dir)
    }
}

/// 讀取並解析 TOML 設定檔。
pub fn load_toml<T: DeserializeOwned>(path: &Path) -> Result<ConfigFile<T>> {
    // 先轉成絕對路徑:`gateway.toml` 這種相對寫法的「所在目錄」才有明確的意思,錯誤訊息也看得出是哪個檔。
    // `std::path::absolute` 不碰檔案系統、不解析 symlink,只是接上目前的工作目錄。
    let path = std::path::absolute(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // 找不到檔案特別處理:呼叫的程式可以據此給出「請複製範例檔」之類的提示。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::NotFound(path));
        }
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    // `T: DeserializeOwned`:T 可以從資料「整個建立出來」,不借用 `text`(`text` 等一下就會被丟掉)。
    // Java 對照:Jackson 的 `mapper.readValue(text, T.class)`,但型別是編譯期決定的泛型參數。
    let value = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.clone(),
        // toml 的錯誤訊息自帶行號、欄號和出錯的那一行,直接沿用。
        message: e.to_string(),
    })?;
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(ConfigFile { value, dir })
}

/// 解析設定裡寫的路徑。`base` 通常是設定檔所在的目錄。
pub fn resolve_path(raw: &str, base: &Path) -> Result<PathBuf> {
    resolve_with(raw, base, || home_from(std::env::var_os("HOME")))
}

/// 預設的資料目錄,例如 macOS 的 `~/Library/Application Support/ai-engineering-lab/gateway`。
pub fn default_data_dir(app: &str) -> Result<PathBuf> {
    data_dir_for(
        cfg!(target_os = "macos"),
        &|name| std::env::var_os(name),
        app,
    )
}

/// 從環境變數讀秘密(API key、token)。沒設定或是空的都算錯。
pub fn secret_env(name: &str) -> Result<SecretString> {
    secret_from(name, std::env::var_os(name))
}

// ---- 內層:值從參數進來,方便測試 ----

// `home` 是一個函式,而不是已經算好的值:只有路徑真的用到 `~` 才去找家目錄。
// 這樣沒設 HOME 的環境(例如某些容器)只要不寫 `~`,照樣能用。
fn resolve_with(raw: &str, base: &Path, home: impl FnOnce() -> Result<PathBuf>) -> Result<PathBuf> {
    let joined = if raw == "~" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else if raw.starts_with('~') {
        // `~bob/...`(別人的家目錄)要查系統的使用者資料庫,不支援;明確拒絕,不要當成相對路徑。
        return Err(ConfigError::UnsupportedTilde(raw.into()));
    } else {
        // `join` 遇到絕對路徑會直接用它;相對路徑才接在 base 後面。`$VAR` 不展開,就是普通的字。
        base.join(raw)
    };
    // `components()` 會把中間的 `.` 拿掉(`/etc/app/./records` → `/etc/app/records`),`..` 保留。
    Ok(joined.components().collect())
}

fn home_from(value: Option<OsString>) -> Result<PathBuf> {
    match value {
        Some(home) if Path::new(&home).is_absolute() => Ok(PathBuf::from(home)),
        // 沒設、空字串、或相對路徑都不可信:寧可報錯,也不要把資料寫到奇怪的地方。
        _ => Err(ConfigError::NoHome),
    }
}

// `env: &dyn Fn(...)`:傳進來的是「查環境變數的方法」。正式版查真的,測試傳一張假的表。
// Java 對照:參數型別是 `Function<String, Optional<String>>`。
fn data_dir_for(macos: bool, env: &dyn Fn(&str) -> Option<OsString>, app: &str) -> Result<PathBuf> {
    let base = if macos {
        home_from(env("HOME"))?.join("Library/Application Support")
    } else {
        // XDG Base Directory 規範:有設、而且是絕對路徑才採用。
        match env("XDG_DATA_HOME") {
            Some(xdg) if Path::new(&xdg).is_absolute() => PathBuf::from(xdg),
            _ => home_from(env("HOME"))?.join(".local/share"),
        }
    };
    Ok(base.join(APP_DIR).join(app))
}

fn secret_from(name: &str, value: Option<OsString>) -> Result<SecretString> {
    let value = value.ok_or_else(|| ConfigError::MissingEnv(name.into()))?;
    // `OsString` 不保證是 UTF-8(Unix 的環境變數可以是任意位元組);header 需要文字,所以要轉換。
    let value = value
        .into_string()
        .map_err(|_| ConfigError::NotUnicodeEnv(name.into()))?;
    if value.trim().is_empty() {
        return Err(ConfigError::EmptyEnv(name.into()));
    }
    // 一拿到就包起來:之後只有明確呼叫 `expose_secret()` 才看得到值,`Debug` 印出 [REDACTED]。
    Ok(SecretString::from(value))
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;
    use serde::Deserialize;

    use super::*;

    fn home() -> Result<PathBuf> {
        Ok(PathBuf::from("/home/u"))
    }

    fn resolve(raw: &str) -> Result<PathBuf> {
        resolve_with(raw, Path::new("/etc/app"), home)
    }

    #[test]
    fn tilde_slash_expands_to_home() {
        assert_eq!(resolve("~/records").unwrap(), Path::new("/home/u/records"));
        assert_eq!(resolve("~").unwrap(), Path::new("/home/u"));
    }

    #[test]
    fn other_tilde_forms_are_rejected() {
        assert!(matches!(
            resolve("~bob/records"),
            Err(ConfigError::UnsupportedTilde(raw)) if raw == "~bob/records"
        ));
    }

    #[test]
    fn environment_variables_are_not_expanded() {
        // 設定檔的內容不該被環境變數悄悄改掉:`$HOME` 就只是一個叫 `$HOME` 的資料夾。
        assert_eq!(
            resolve("$HOME/records").unwrap(),
            Path::new("/etc/app/$HOME/records")
        );
    }

    #[test]
    fn relative_paths_are_relative_to_the_config_directory() {
        // 比字串而不是比 Path:`Path` 的 `==` 比的是組成部分,中間的 `.` 本來就會被忽略,
        // `/etc/app/./records == /etc/app/records` 是 true。整理 `./` 是為了印給人看的路徑乾淨。
        assert_eq!(
            resolve("./records").unwrap().to_str(),
            Some("/etc/app/records")
        );
        assert_eq!(resolve("records").unwrap(), Path::new("/etc/app/records"));
        // `..` 保留原樣:是否允許往上走,由使用的程式決定。
        assert_eq!(resolve("../data").unwrap(), Path::new("/etc/app/../data"));
    }

    #[test]
    fn absolute_paths_are_kept() {
        assert_eq!(resolve("/var/lib/r").unwrap(), Path::new("/var/lib/r"));
    }

    #[test]
    fn home_is_only_needed_when_the_path_uses_it() {
        let no_home = || Err(ConfigError::NoHome);
        assert!(resolve_with("records", Path::new("/etc/app"), no_home).is_ok());
        assert!(matches!(
            resolve_with("~/records", Path::new("/etc/app"), no_home),
            Err(ConfigError::NoHome)
        ));
    }

    #[test]
    fn home_must_be_set_and_absolute() {
        assert_eq!(
            home_from(Some("/home/u".into())).unwrap(),
            Path::new("/home/u")
        );
        assert!(matches!(home_from(None), Err(ConfigError::NoHome)));
        assert!(matches!(
            home_from(Some("".into())),
            Err(ConfigError::NoHome)
        ));
        assert!(matches!(
            home_from(Some("relative".into())),
            Err(ConfigError::NoHome)
        ));
    }

    /// 假的環境變數表。
    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn data_dir_on_macos_uses_application_support() {
        let dir = data_dir_for(true, &env(&[("HOME", "/Users/u")]), "gateway").unwrap();
        assert_eq!(
            dir,
            Path::new("/Users/u/Library/Application Support/ai-engineering-lab/gateway")
        );
    }

    #[test]
    fn data_dir_elsewhere_follows_xdg() {
        let home_only = data_dir_for(false, &env(&[("HOME", "/home/u")]), "gateway").unwrap();
        assert_eq!(
            home_only,
            Path::new("/home/u/.local/share/ai-engineering-lab/gateway")
        );

        let with_xdg = env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(
            data_dir_for(false, &with_xdg, "gateway").unwrap(),
            Path::new("/data/ai-engineering-lab/gateway")
        );

        // XDG 規範:相對路徑的 XDG_DATA_HOME 無效,要忽略。
        let relative_xdg = env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "data")]);
        assert_eq!(
            data_dir_for(false, &relative_xdg, "gateway").unwrap(),
            home_only
        );
    }

    #[test]
    fn data_dir_without_home_is_an_error() {
        assert!(matches!(
            data_dir_for(true, &env(&[]), "gateway"),
            Err(ConfigError::NoHome)
        ));
    }

    #[test]
    fn secrets_come_wrapped_and_never_appear_in_errors() {
        let secret = secret_from("GATEWAY_TOKEN", Some("s3cr3t".into())).unwrap();
        assert_eq!(secret.expose_secret(), "s3cr3t");
        assert!(!format!("{secret:?}").contains("s3cr3t"), "Debug 要遮掉");

        let missing = secret_from("GATEWAY_TOKEN", None).unwrap_err();
        assert_eq!(
            missing.to_string(),
            "environment variable GATEWAY_TOKEN is not set"
        );
        assert!(matches!(
            secret_from("GATEWAY_TOKEN", Some("   ".into())),
            Err(ConfigError::EmptyEnv(_))
        ));
    }

    #[derive(Debug, PartialEq, Deserialize)]
    // 不認得的欄位直接報錯:打成 `max_atempts` 不能被默默忽略(D-024)。
    #[serde(deny_unknown_fields)]
    struct Example {
        name: String,
        dir: String,
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn loads_a_file_and_resolves_paths_against_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let conf_dir = tmp.path().join("conf");
        let path = write(
            &conf_dir,
            "app.toml",
            "name = \"lab\"\ndir = \"./records\"\n",
        );

        let loaded: ConfigFile<Example> = load_toml(&path).unwrap();

        assert_eq!(
            loaded.value,
            Example {
                name: "lab".into(),
                dir: "./records".into()
            }
        );
        assert_eq!(loaded.dir, conf_dir);
        // 不管從哪個目錄啟動程式,結果都一樣。
        assert_eq!(
            loaded.resolve(&loaded.value.dir).unwrap(),
            conf_dir.join("records")
        );
    }

    #[test]
    fn unknown_field_error_names_the_file_the_line_and_the_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            "app.toml",
            "name = \"lab\"\ndir = \"r\"\nmax_atempts = 5\n",
        );

        let err = load_toml::<Example>(&path).unwrap_err();

        let message = err.to_string();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
        assert!(message.contains("app.toml"), "{message}");
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains("max_atempts"), "{message}");
    }

    #[test]
    fn other_read_errors_are_not_mistaken_for_not_found() {
        // 路徑存在但讀不了(這裡是目錄):不能說成「找不到」,否則使用者會被提示去複製範例檔,蓋掉自己的東西。
        // (cargo-mutants:NotFound 的判斷換成 `true` 時沒有測試失敗。)
        let tmp = tempfile::tempdir().unwrap();

        let err = load_toml::<Example>(tmp.path()).unwrap_err();

        assert!(matches!(&err, ConfigError::Read { .. }), "{err:?}");
    }

    #[test]
    fn default_data_dir_reads_the_real_environment() {
        // 外層只是把真的環境變數接到內層(內層 `data_dir_for` 已經測過各種情況)。
        // 這裡只讀、不改環境變數,所以不需要 unsafe 的 `set_var`。
        // (cargo-mutants:外層換成回傳空路徑時沒有測試失敗。)
        let dir = default_data_dir("unit-test-app").unwrap();
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(
            dir.ends_with("ai-engineering-lab/unit-test-app"),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn secret_env_reads_the_real_environment() {
        // 一定存在的變數(PATH)與一定不存在的變數,各讀一次。(cargo-mutants:外層換成回傳空秘密時沒有測試失敗。)
        let path = secret_env("PATH").unwrap();
        assert_eq!(
            Some(path.expose_secret()),
            std::env::var("PATH").ok().as_deref()
        );
        assert!(secret_env("AI_LAB_TEST_SURELY_UNSET_7F3A").is_err());
    }

    #[test]
    fn missing_file_is_reported_as_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.toml");

        let err = load_toml::<Example>(&path).unwrap_err();

        assert!(
            matches!(&err, ConfigError::NotFound(p) if *p == path),
            "{err:?}"
        );
    }
}
