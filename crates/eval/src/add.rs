//! `eval add`:從 gateway 的呼叫紀錄挑一筆,變成私有資料集的案例(design §4、D-030)。
//!
//! ```text
//! 紀錄 id(UUID v7)──取出時間──→ 那一天的紀錄檔(2026-11-01.jsonl)──找到那一筆──→ CallRecord
//!   ──請求 → 案例的輸入;評分標準由使用者給;--keep-answer:原本的回答 → 參考答案──→ Case
//!   ──檢查(案例本身、id 不重複、目錄 700 / 檔案 600)──→ 附加到 <data_dir>/datasets/<名稱>.jsonl
//! ```
//!
//! 只寫私有資料集:真實案例是完整的個人對話,不能進公開 repo(D-030)。

use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use llm::{CallRecord, Outcome};
use uuid::Uuid;

use crate::case::{Case, ChatInput, Input, Output};
use crate::dataset::{Dataset, DatasetError};

/// 案例要加上的東西:紀錄裡沒有的部分由使用者給。
#[derive(Debug, Clone, Default)]
pub struct CaseOptions {
    /// 案例 id;沒給就用 `rec-<紀錄 id>`(找得回原本的紀錄,也保證不重複)。
    pub id: Option<String>,
    pub tags: Vec<String>,
    pub rubric: Vec<String>,
    /// 把原本的回答存成參考答案(只給評審看)。
    pub keep_answer: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0} is not a call record id (call records use UUID v7)")]
    NotCallRecordId(Uuid),

    #[error("call record {id} not found; looked in {}", looked.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))]
    RecordNotFound { id: Uuid, looked: Vec<PathBuf> },

    #[error("cannot read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "call record {id} has no complete answer to keep (outcome: {outcome:?}); leave out --keep-answer"
    )]
    NoAnswer { id: Uuid, outcome: Outcome },

    #[error("the new case is invalid:\n{}", .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
    InvalidCase(Vec<String>),

    #[error(transparent)]
    Dataset(#[from] DatasetError),

    #[error("case `{id}` already exists in {}; pick another --id", path.display())]
    DuplicateId { id: String, path: PathBuf },

    #[error("{} is accessible by other users (mode {mode:o}); run `chmod {fix}` on it", path.display())]
    NotPrivate {
        path: PathBuf,
        mode: u32,
        fix: &'static str,
    },

    #[error("cannot write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// 這筆紀錄可能在哪幾個檔案:id 的時間那一天,再加上前後一天。
///
/// 紀錄依「開始時間」(UTC)分檔,id 是在同一個時刻產生的,所以幾乎都在第一個檔案;
/// 前後一天是保險:午夜前後幾毫秒,兩個時間可能落在不同天。
pub fn record_files(dir: &Path, id: Uuid) -> Result<Vec<PathBuf>, AddError> {
    // `get_version_num`:UUID 的版本寫在固定的幾個位元裡。v7 的前 48 位元是毫秒時間戳;
    // v4 是隨機的,沒有時間可以取(Java 對照:`UUID.version()`)。
    let time = (id.get_version_num() == 7)
        .then(|| id.get_timestamp())
        .flatten()
        .ok_or(AddError::NotCallRecordId(id))?;
    let (secs, nanos) = time.to_unix();
    let at = i64::try_from(secs)
        .ok()
        .and_then(|secs| Timestamp::new(secs, i32::try_from(nanos).ok()?).ok())
        .ok_or(AddError::NotCallRecordId(id))?;
    let day = SignedDuration::from_hours(24);
    // `checked_sub` / `checked_add`:超出時間能表示的範圍時是 None(這裡不可能,但不 panic)。
    let days = [Some(at), at.checked_sub(day).ok(), at.checked_add(day).ok()];
    // 檔名跟 gateway 的 JsonlWriter 一樣:UTC 的日期。
    Ok(days
        .into_iter()
        .flatten()
        .map(|t| dir.join(format!("{}.jsonl", t.strftime("%Y-%m-%d"))))
        .collect())
}

/// 在紀錄目錄裡找到這一筆。寫到一半的行(gateway 當機時留下的)讀不懂就略過(同 D-021 的讀取規則)。
pub fn find_record(dir: &Path, id: Uuid) -> Result<CallRecord, AddError> {
    let files = record_files(dir, id)?;
    for path in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // 那天沒有紀錄:看下一個檔案。
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(AddError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        // `find_map`:找到第一個符合的就停,回傳它。Java 對照:`stream().map(...).filter(...).findFirst()`。
        let found = text
            .lines()
            .filter_map(|line| serde_json::from_str::<CallRecord>(line).ok())
            .find(|record| record.id == id);
        if let Some(record) = found {
            return Ok(record);
        }
    }
    Err(AddError::RecordNotFound { id, looked: files })
}

/// 把紀錄變成案例:請求的內容(去掉模型)就是案例的輸入。
pub fn case_from_record(record: &CallRecord, options: &CaseOptions) -> Result<Case, AddError> {
    // 解構:把 request 的欄位一次拿出來。`model: _` 明確寫出「這個欄位故意不要」:
    // 案例不指定模型(design §4),之後 Request 多了欄位,這裡會編譯失敗,提醒要決定新欄位要不要帶。
    let llm::Request {
        model: _,
        system,
        messages,
        tools,
        max_tokens,
        provider_options,
    } = record.request.clone();
    let reference = if options.keep_answer {
        let answer = match (&record.outcome, &record.response) {
            (Outcome::Ok, Some(response)) => Output::Chat(response.clone()).text(),
            _ => String::new(),
        };
        // 中斷、失敗、或沒有文字:不是完整的回答,不能當參考答案。
        if answer.trim().is_empty() {
            return Err(AddError::NoAnswer {
                id: record.id,
                outcome: record.outcome,
            });
        }
        Some(answer)
    } else {
        None
    };
    let case = Case {
        id: options
            .id
            .clone()
            .unwrap_or_else(|| format!("rec-{}", record.id)),
        tags: options.tags.clone(),
        input: Input::Chat(ChatInput {
            system,
            messages,
            tools,
            max_tokens,
            provider_options,
        }),
        checks: vec![],
        rubric: options.rubric.clone(),
        reference,
    };
    // 跟讀資料集時同一套檢查:寫進去的案例,下次讀檔一定讀得懂。
    let problems = case.problems();
    if problems.is_empty() {
        Ok(case)
    } else {
        Err(AddError::InvalidCase(problems))
    }
}

/// 附加到資料集檔案。檔案與目錄不存在就建立(700 / 600);既有的資料集要先讀得懂、id 不重複,才寫入(寫入路徑 fail-loud)。
pub fn append_case(path: &Path, case: &Case) -> Result<(), AddError> {
    if let Some(dir) = path.parent() {
        // `mode(0o700)`:建立的那一刻就設好權限(同 store.rs)。
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| AddError::Write {
                path: dir.to_path_buf(),
                source,
            })?;
        check_private(dir, "700")?;
    }
    if path.exists() {
        check_private(path, "600")?;
        // 既有的資料集讀不懂就不往後加:不猜,也不把它弄得更亂。
        let existing = Dataset::load(path)?;
        if existing.cases.iter().any(|c| c.id == case.id) {
            return Err(AddError::DuplicateId {
                id: case.id.clone(),
                path: path.to_path_buf(),
            });
        }
    }
    let write = |source| AddError::Write {
        path: path.to_path_buf(),
        source,
    };
    let mut line = serde_json::to_vec(case).map_err(|e| write(io::Error::other(e)))?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(write)?;
    // 手寫的資料集最後一行常常沒有換行:先補一個,新的一行才不會黏在後面。
    if ends_without_newline(&mut file).map_err(write)? {
        file.write_all(b"\n").map_err(write)?;
    }
    file.write_all(&line).map_err(write)
}

fn ends_without_newline(file: &mut std::fs::File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last != *b"\n")
}

/// 只有自己能讀:群組與其他人的位元都不能開。
fn check_private(path: &Path, fix: &'static str) -> Result<(), AddError> {
    let mode = std::fs::metadata(path)
        .map_err(|source| AddError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(AddError::NotPrivate {
            path: path.to_path_buf(),
            mode,
            fix,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
