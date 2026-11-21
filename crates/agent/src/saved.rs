//! 暫停的執行存成檔案(003 第三部分第 4 步,D-057):`agent run` 在核准提示回答 `s` 時存檔,`agent resume <run id>` 讀回來接續。
//!
//! 檔案在 `<runs>/suspended/<run id>.json`:目錄 700、檔案 600,內容等同軌跡(對話、工具結果,D-017)。
//! 除了 `Suspended`(agent loop 的狀態),還存 CLI 這一層的選項(`RunPlan`):接續時要用**一樣的**工具與設定重建 agent,
//! `Agent::resume` 再確認一次真的一樣。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::run::Suspended;
use crate::trajectory::ensure_private_dir;

/// 存檔在軌跡目錄底下的這個子目錄。
pub const SUSPENDED_DIR: &str = "suspended";

/// 存檔的大小上限。對話最多是幾輪的模型回應加工具結果(每個結果 32 KiB 以內),16 MiB 很寬鬆。
const MAX_SAVED_BYTES: u64 = 16 * 1024 * 1024;

/// 建立 agent 用的選項:`agent run` 從命令列來,`agent resume` 從存檔來。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    /// 模型別名(`[aliases]` 裡的名字)。
    pub model: String,
    /// system prompt 的**內容**(不是檔名:檔案之後可能被改)。
    pub system: String,
    pub max_turns: Option<u32>,
    /// 檔案工具的根目錄(絕對路徑:接續時的目前目錄可能不同)。
    pub root: Option<PathBuf>,
    pub allow_write: bool,
    /// 模型呼叫要不要寫進呼叫紀錄(`--no-record` 的反面)。
    pub record: bool,
}

/// 一個存檔。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRun {
    pub plan: RunPlan,
    /// 接著寫的軌跡檔。
    pub trajectory: PathBuf,
    pub suspended: Suspended,
}

/// 讀不到存檔。
#[derive(Debug, thiserror::Error)]
pub enum SavedError {
    #[error("there is no paused run {0}")]
    NotFound(Uuid),
    #[error("the saved run {path} cannot be read: {reason}")]
    Unreadable { path: PathBuf, reason: String },
}

/// 存檔的路徑。run id 是 `Uuid`(不是任意字串),所以組不出 `../` 之類的路徑。
pub fn path(dir: &Path, run_id: Uuid) -> PathBuf {
    dir.join(format!("{run_id}.json"))
}

/// 寫一個存檔(目錄 700、檔案 600,建立的那一刻就是 600)。同一個 run id 已經有存檔就失敗(不覆寫)。
pub fn save(dir: &Path, saved: &SavedRun) -> io::Result<PathBuf> {
    ensure_private_dir(dir)?;
    let path = path(dir, saved.suspended.run_id);
    let json = serde_json::to_vec_pretty(saved).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path)?;
    file.write_all(&json)?;
    file.sync_all()?;
    Ok(path)
}

/// 讀存檔(還不刪:接續前要先確認設定沒變,不行的話使用者改好設定可以再試)。
pub fn load(dir: &Path, run_id: Uuid) -> Result<SavedRun, SavedError> {
    let path = path(dir, run_id);
    let unreadable = |reason: String| SavedError::Unreadable {
        path: path.clone(),
        reason,
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(SavedError::NotFound(run_id)),
        Err(e) => return Err(unreadable(e.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SAVED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| unreadable(e.to_string()))?;
    if bytes.len() as u64 > MAX_SAVED_BYTES {
        return Err(unreadable("it is larger than 16 MiB".into()));
    }
    serde_json::from_slice(&bytes).map_err(|e| unreadable(e.to_string()))
}

/// 認領存檔:刪掉它。在執行任何東西**之前**做 —— 兩個 `agent resume` 同時接續同一個存檔時,
/// 只有一個刪得掉(刪除是原子的),另一個失敗,核准過的寫入不會被執行兩次。
pub fn claim(dir: &Path, run_id: Uuid) -> Result<(), SavedError> {
    match std::fs::remove_file(path(dir, run_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(SavedError::NotFound(run_id)),
        Err(e) => Err(SavedError::Unreadable {
            path: path(dir, run_id),
            reason: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests;
