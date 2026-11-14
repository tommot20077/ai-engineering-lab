//! 軌跡(design §6):一次執行寫成一個 JSONL 檔,一行一個事件。
//!
//! 內容含模型的完整回應、工具的參數與結果(`read_file` 讀到的檔案也在裡面),所以跟呼叫紀錄一樣只存本機:
//! 目錄 700、檔案 600(D-017)。

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use llm::Usage;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::limits::AgentLimits;
use crate::run::{Outcome, Step};

/// 軌跡的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Start {
        run_id: Uuid,
        started_at: Timestamp,
        /// 請求的模型名稱(別名);真正回答的模型在每一步的 `response.model`。
        model: String,
        task: String,
        tools: Vec<String>,
        limits: AgentLimits,
    },
    Step {
        step: Step,
    },
    Finish {
        outcome: Outcome,
        usage: Usage,
        /// 沒有價格時是 `null`(未知,不是 0)。
        cost_usd: Option<f64>,
    },
}

/// 寫一次執行的軌跡。
pub struct TrajectoryWriter {
    path: PathBuf,
    file: File,
}

impl TrajectoryWriter {
    /// 在 `dir` 底下建立 `<開始時間>_<run id>.jsonl`。目錄不存在就建立(700);別人讀得到就拒絕。
    pub fn create(dir: &Path, started_at: Timestamp, run_id: Uuid) -> io::Result<TrajectoryWriter> {
        ensure_private_dir(dir)?;
        let name = format!("{}_{run_id}.jsonl", started_at.strftime("%Y%m%dT%H%M%SZ"));
        let path = dir.join(name);
        let mut options = OpenOptions::new();
        // `create_new`:檔案已經存在就失敗,不覆寫別人的軌跡。
        options.write(true).create_new(true);
        // 建立的那一刻就是 600,不是先建立再 chmod(中間會有空窗)。
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        Ok(TrajectoryWriter { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 寫一行、立刻 flush:程式中途被中止,已經發生的步驟還在檔案裡。
    pub fn write(&mut self, event: &Event) -> io::Result<()> {
        let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
        line.push('\n');
        // 一次寫入整行(同 D-021 的呼叫紀錄):讀取端不會看到半行混著另一行。
        self.file.write_all(line.as_bytes())?;
        self.file.flush()
    }
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)?;
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        // `0o077`:群組與其他人的讀、寫、執行位元。任何一個有開,就不是「只有自己」。
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is accessible by other users (mode {mode:o}); run `chmod 700` on it",
                    dir.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> Timestamp {
        "2026-11-14T02:30:00Z".parse().unwrap()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn events_are_written_one_per_line_to_a_private_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agent/runs");
        let run_id = Uuid::nil();
        let mut writer = TrajectoryWriter::create(&dir, at(), run_id).unwrap();
        assert_eq!(
            writer.path(),
            dir.join("20261114T023000Z_00000000-0000-0000-0000-000000000000.jsonl")
        );
        let finish = Event::Finish {
            outcome: Outcome::Refused,
            usage: Usage::default(),
            cost_usd: None,
        };
        writer.write(&finish).unwrap();
        writer.write(&finish).unwrap();
        let text = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        let back: Event = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back, finish);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0]).unwrap()["type"],
            "finish"
        );
        #[cfg(unix)]
        {
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(writer.path()), 0o600);
        }
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        // 用子目錄:暫存目錄本身可能是 755,會被「別人讀得到」的檢查擋下。
        let dir = tmp.path().join("runs");
        TrajectoryWriter::create(&dir, at(), Uuid::nil()).unwrap();
        let again = TrajectoryWriter::create(&dir, at(), Uuid::nil())
            .err()
            .unwrap();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_others_can_read_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("open");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        let error = TrajectoryWriter::create(&dir, at(), Uuid::nil())
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("mode 750"), "{error}");
    }
}
