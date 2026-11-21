//! 設定檔裡的 `[sandbox]` 段落(agent.toml、eval.toml 共用)。
//!
//! 單位是給人寫的(秒、MiB、KiB);`to_config` 換成 `ContainerConfig` 並檢查(上限是 0、image 沒有鎖定 → 錯誤)。
//! 這個 crate 不讀檔案:讀檔、找路徑是呼叫端(config-loader)的事,這裡只定義形狀。

use std::time::Duration;

use serde::Deserialize;

use crate::{
    ContainerConfig, ContainerSandbox, DEFAULT_IMAGE, DEFAULT_PROGRAM, Limits, SandboxError,
};

/// `[sandbox]`。每個欄位都可以省略(用預設值);打錯的欄位名稱會被拒絕(`deny_unknown_fields`)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxSection {
    /// `docker` 或 `podman`。
    pub program: String,
    /// 必須用 digest 鎖定。
    pub image: String,
    pub compile_timeout_secs: u64,
    pub run_timeout_secs: u64,
    pub memory_mib: u64,
    pub pids: u32,
    /// stdout、stderr 各自保留多少。
    pub output_kib: u64,
    /// 工作目錄(tmpfs)的大小。
    pub scratch_mib: u64,
    /// 原始碼的大小上限。
    pub code_kib: u64,
}

impl Default for SandboxSection {
    fn default() -> Self {
        let limits = Limits::default();
        Self {
            program: DEFAULT_PROGRAM.to_owned(),
            image: DEFAULT_IMAGE.to_owned(),
            compile_timeout_secs: limits.compile_timeout.as_secs(),
            run_timeout_secs: limits.run_timeout.as_secs(),
            memory_mib: limits.memory_bytes / MIB,
            pids: limits.pids,
            output_kib: limits.output_bytes as u64 / KIB,
            scratch_mib: limits.scratch_bytes / MIB,
            code_kib: limits.code_bytes as u64 / KIB,
        }
    }
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

impl SandboxSection {
    /// 換成 `ContainerConfig`,並用 `ContainerSandbox::new` 的規則檢查(同一套規則,只寫一次)。
    pub fn to_config(&self) -> Result<ContainerConfig, SandboxError> {
        let bytes = |value: u64, unit: u64, field: &str| {
            value
                .checked_mul(unit)
                .ok_or_else(|| SandboxError::InvalidConfig(format!("sandbox.{field} is too large")))
        };
        let usize_bytes = |value: u64, unit: u64, field: &str| {
            bytes(value, unit, field).and_then(|b| {
                usize::try_from(b).map_err(|_| {
                    SandboxError::InvalidConfig(format!("sandbox.{field} is too large"))
                })
            })
        };
        let config = ContainerConfig {
            program: self.program.clone(),
            image: self.image.clone(),
            limits: Limits {
                compile_timeout: Duration::from_secs(self.compile_timeout_secs),
                run_timeout: Duration::from_secs(self.run_timeout_secs),
                memory_bytes: bytes(self.memory_mib, MIB, "memory_mib")?,
                pids: self.pids,
                output_bytes: usize_bytes(self.output_kib, KIB, "output_kib")?,
                scratch_bytes: bytes(self.scratch_mib, MIB, "scratch_mib")?,
                code_bytes: usize_bytes(self.code_kib, KIB, "code_kib")?,
            },
        };
        ContainerSandbox::new(config.clone())?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<SandboxSection, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn an_empty_section_is_the_default_config() {
        let section = parse("").unwrap();
        assert_eq!(section.to_config().unwrap(), ContainerConfig::default());
        assert_eq!(
            (
                section.compile_timeout_secs,
                section.run_timeout_secs,
                section.memory_mib
            ),
            (20, 5, 512)
        );
        assert_eq!(
            (
                section.pids,
                section.output_kib,
                section.scratch_mib,
                section.code_kib
            ),
            (64, 64, 64, 64)
        );
    }

    #[test]
    fn every_field_converts_to_its_unit() {
        let section = parse(
            r#"
            program = "podman"
            compile_timeout_secs = 30
            run_timeout_secs = 2
            memory_mib = 256
            pids = 32
            output_kib = 8
            scratch_mib = 16
            code_kib = 4
            "#,
        )
        .unwrap();
        let config = section.to_config().unwrap();
        assert_eq!(config.program, "podman");
        assert_eq!(config.image, DEFAULT_IMAGE);
        assert_eq!(
            config.limits,
            Limits {
                compile_timeout: Duration::from_secs(30),
                run_timeout: Duration::from_secs(2),
                memory_bytes: 256 * MIB,
                pids: 32,
                output_bytes: 8 * 1024,
                scratch_bytes: 16 * MIB,
                code_bytes: 4 * 1024,
            }
        );
    }

    #[test]
    fn the_same_rules_as_the_sandbox_apply() {
        let zero = parse("run_timeout_secs = 0")
            .unwrap()
            .to_config()
            .unwrap_err();
        assert_eq!(
            zero.to_string(),
            "invalid sandbox configuration: the sandbox limit `run_timeout` must be greater than 0"
        );
        let unpinned = parse(r#"image = "rust:latest""#)
            .unwrap()
            .to_config()
            .unwrap_err();
        assert!(
            unpinned.to_string().contains("must be pinned by digest"),
            "{unpinned}"
        );
        let huge = parse("memory_mib = 18446744073709551615")
            .unwrap()
            .to_config()
            .unwrap_err();
        assert_eq!(
            huge,
            SandboxError::InvalidConfig("sandbox.memory_mib is too large".into())
        );
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(parse("network = true").is_err());
    }
}
