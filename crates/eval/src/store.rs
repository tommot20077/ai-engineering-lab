//! 一次執行的目錄(design §5、D-032)。
//!
//! ```text
//! <資料目錄>/eval/runs/                       700
//!   20261101T120000Z_rust-basics_cheap/       700  一次執行
//!     meta.json                               600  設定與摘要
//!     responses.jsonl                         600  一行一次呼叫,只往後加
//!     scores/                                      打分的結果(第 5 步)
//! ```
//!
//! 結果裡是模型的回答,私有資料集的話就是真實對話的延伸:跟 001 的呼叫紀錄一樣,只有自己讀得到。

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use llm::RecordedError;
use serde::{Deserialize, Serialize};

use crate::case::Output;
use crate::run::{RunConfig, Summary};

const META: &str = "meta.json";
const RESPONSES: &str = "responses.jsonl";

/// `meta.json`:這次執行「是什麼」。接續(`--resume`)時,設定全部從這裡讀回來。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    pub eval_version: String,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    pub dataset: DatasetInfo,
    pub config: RunConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// 每次執行(含接續)結束時更新。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetInfo {
    pub name: String,
    /// 接續時從這裡重新讀資料集,並用 `sha256` 確認內容沒變。
    pub path: PathBuf,
    pub sha256: String,
    pub cases: usize,
}

/// `responses.jsonl` 的一行:一次呼叫的結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseLine {
    pub case_id: String,
    /// 第幾次抽樣(從 0 開始)。
    pub sample: u32,
    // `flatten`:把 `CallResult` 的欄位攤平在同一層,JSON 裡是 `"outcome": "ok", "output": {...}`,不多包一層。
    #[serde(flatten)]
    pub result: CallResult,
    pub latency_ms: u64,
    /// 沒有價格時是 `None`(顯示「未知」,不是 0)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CallResult {
    Ok {
        output: Output,
    },
    /// 失敗也要記(design §5):跳過失敗的題目會讓分數虛高。分類沿用 001 呼叫紀錄的 `RecordedError`。
    Error {
        error: RecordedError,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{} is accessible by other users (mode {mode:o}); run `chmod 700` on it", path.display())]
    NotPrivate { path: PathBuf, mode: u32 },

    #[error("{}, line {line}: {message}", path.display())]
    Corrupt {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

/// 讓 `?` 能順手加上路徑:`fs::read(&p).at(&p)?`。
trait At<T> {
    fn at(self, path: &Path) -> Result<T, StoreError>;
}

// 替標準庫的 `Result` 加一個方法:Rust 允許替別人的型別實作「自己的」trait(Java 做不到,只能寫工具類別)。
impl<T> At<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T, StoreError> {
        self.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// 一次執行的目錄。
#[derive(Debug)]
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    /// 在 `root` 底下建立新的執行目錄,寫入 `meta.json`。`root` 不存在就建立(700);
    /// 已經存在但別人也讀得到,就拒絕(D-021 的同一條規則)。
    pub fn create(root: &Path, meta: &RunMeta) -> Result<Self, StoreError> {
        ensure_private_dir(root)?;
        let name = format!(
            "{}_{}_{}",
            meta.started_at.strftime("%Y%m%dT%H%M%SZ"),
            safe_name(&meta.dataset.name),
            safe_name(&meta.config.model)
        );
        let path = root.join(name);
        // 不加 `recursive`:目錄已經存在就是錯誤(同一秒、同一組設定跑了兩次),不能把兩次執行混在一起。
        DirBuilder::new().mode(0o700).create(&path).at(&path)?;
        let dir = Self { path };
        dir.write_meta(meta)?;
        Ok(dir)
    }

    /// 打開既有的執行目錄(接續、打分、比較時用)。
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        check_private(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_meta(&self) -> Result<RunMeta, StoreError> {
        let path = self.path.join(META);
        let text = std::fs::read_to_string(&path).at(&path)?;
        serde_json::from_str(&text).map_err(|e| StoreError::Corrupt {
            path,
            line: e.line(),
            message: e.to_string(),
        })
    }

    /// 寫入 `meta.json`。先寫到暫存檔再改名:改名是原子操作,中途當掉也不會留下寫了一半的 meta。
    pub fn write_meta(&self, meta: &RunMeta) -> Result<(), StoreError> {
        let path = self.path.join(META);
        let tmp = self.path.join("meta.json.tmp");
        let mut text = serde_json::to_vec_pretty(meta)
            .map_err(io::Error::other)
            .at(&tmp)?;
        text.push(b'\n');
        let mut file = private_file(&tmp, false)?;
        file.write_all(&text).at(&tmp)?;
        // Java 對照:`Files.move(tmp, path, ATOMIC_MOVE)`。
        std::fs::rename(&tmp, &path).at(&path)
    }

    /// 讀出全部的結果。最後一行如果沒有換行(上次寫到一半就當掉),那一行不算,當成沒做過。
    pub fn read_responses(&self) -> Result<Vec<ResponseLine>, StoreError> {
        let path = self.path.join(RESPONSES);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // 還沒有任何結果(例如剛建立就中斷)不是錯誤。
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(StoreError::Io { path, source: e }),
        };
        // `split_inclusive('\n')`:每一段都保留結尾的換行,最後一段沒有換行就是寫到一半的那行。
        let mut lines = Vec::new();
        for (index, piece) in text.split_inclusive('\n').enumerate() {
            let Some(line) = piece.strip_suffix('\n') else {
                break; // 沒有換行:上次寫到一半就當掉,當成沒做過。
            };
            if line.trim().is_empty() {
                continue;
            }
            let parsed = serde_json::from_str(line).map_err(|e| StoreError::Corrupt {
                path: path.clone(),
                line: index + 1,
                message: e.to_string(),
            })?;
            lines.push(parsed);
        }
        Ok(lines)
    }

    /// 打開 `responses.jsonl` 準備往後加。
    pub fn appender(&self) -> Result<Appender, StoreError> {
        let path = self.path.join(RESPONSES);
        let mut file = private_file(&path, true)?;
        seal_partial_line(&mut file).at(&path)?;
        Ok(Appender { file, path })
    }
}

/// 往 `responses.jsonl` 後面加一行。
pub struct Appender {
    file: File,
    path: PathBuf,
}

impl Appender {
    pub fn append(&mut self, line: &ResponseLine) -> Result<(), StoreError> {
        // 先組好完整的一行再一次寫出去:不會出現兩筆交錯在同一行(同 001 的 `JsonlWriter`)。
        let mut bytes = serde_json::to_vec(line)
            .map_err(io::Error::other)
            .at(&self.path)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes).at(&self.path)
    }
}

/// 開檔(600)。`append`:往後加;否則清空重寫。
fn private_file(path: &Path, append: bool) -> Result<File, StoreError> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .append(append)
        .write(!append)
        .truncate(!append)
        .mode(0o600)
        .open(path)
        .at(path)
}

/// 建立目錄(700),或確認既有目錄只有自己能進入。
// 跟 llm `JsonlWriter` 的 `prepare_dir` 同一條規則;那邊是私有函式,這裡另寫一份(見 ROADMAP 待辦)。
fn ensure_private_dir(dir: &Path) -> Result<(), StoreError> {
    // `mode(0o700)`:在建立的那一刻就設好權限,不是先建立再 chmod(中間會有空窗)。
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .at(dir)?;
    check_private(dir)
}

fn check_private(dir: &Path) -> Result<(), StoreError> {
    let mode = std::fs::metadata(dir).at(dir)?.permissions().mode() & 0o777;
    // `0o077`:群組與其他人的讀、寫、執行位元。任何一個有開,就不是「只有自己」。
    if mode & 0o077 != 0 {
        return Err(StoreError::NotPrivate {
            path: dir.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// 上次當機可能留下寫了一半的行。最後一個字元不是換行就先補一個,下一筆才不會黏在半行後面。
fn seal_partial_line(file: &mut File) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    // 以 append 模式打開的檔案也能讀:移到最後一個 byte 看看它是不是換行。寫入一律加在最後,不受 seek 影響。
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last != *b"\n" {
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// 目錄名稱只留英數字與 `.`、`_`、`-`,其他換成 `-`:模型或資料集的名字不能讓路徑跑到別的地方(例如 `../`)。
fn safe_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use llm::{Content, ProviderMetadata, Response, StopKind, StopReason, Usage};
    use serde_json::json;

    use super::*;

    fn meta(model: &str) -> RunMeta {
        RunMeta {
            eval_version: "0.1.0".into(),
            started_at: "2026-11-01T12:00:00Z".parse().unwrap(),
            finished_at: None,
            dataset: DatasetInfo {
                name: "rust-basics".into(),
                path: "/data/rust-basics.jsonl".into(),
                sha256: "abc".into(),
                cases: 2,
            },
            config: RunConfig {
                model: model.into(),
                repeats: 1,
                tags: vec![],
                limit: None,
                label: None,
            },
            git_commit: None,
            summary: None,
        }
    }

    fn ok_line(case_id: &str, sample: u32) -> ResponseLine {
        ResponseLine {
            case_id: case_id.into(),
            sample,
            result: CallResult::Ok {
                output: Output::Chat(Response {
                    id: None,
                    model: "claude-haiku-5-5".into(),
                    content: vec![Content::Text {
                        text: "hi".into(),
                        meta: ProviderMetadata::default(),
                    }],
                    stop: StopReason {
                        kind: StopKind::EndTurn,
                        raw: "end_turn".into(),
                    },
                    usage: Usage::default(),
                    meta: ProviderMetadata::default(),
                }),
            },
            latency_ms: 12,
            cost_usd: Some(0.5),
        }
    }

    // 注意:`tempfile` 建立的暫存目錄是 755(別人讀得到),直接拿它當 runs 目錄會被正確地拒絕。
    // 所以測試一律用它底下的 `runs`,由程式自己建立(700)。
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn create_makes_a_private_directory_named_after_time_dataset_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("runs");

        let dir = RunDir::create(&root, &meta("cheap")).unwrap();

        assert_eq!(dir.path(), root.join("20261101T120000Z_rust-basics_cheap"));
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(dir.path()), 0o700);
        assert_eq!(mode(&dir.path().join("meta.json")), 0o600);
        assert_eq!(dir.read_meta().unwrap(), meta("cheap"));
    }

    #[test]
    fn a_runs_directory_others_can_read_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("runs");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = RunDir::create(&root, &meta("cheap")).unwrap_err();

        assert!(
            matches!(&err, StoreError::NotPrivate { path, mode: 0o755 } if *path == root),
            "{err:?}"
        );
        // 打開既有的執行目錄也一樣檢查。
        assert!(matches!(
            RunDir::open(&root),
            Err(StoreError::NotPrivate { .. })
        ));
    }

    #[test]
    fn the_same_run_cannot_be_created_twice() {
        // 同一秒、同一組設定:不能把兩次執行混在同一個目錄。
        let tmp = tempfile::tempdir().unwrap();
        RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap();
        let err = RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap_err();
        assert!(
            matches!(&err, StoreError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists),
            "{err:?}"
        );
    }

    #[test]
    fn names_cannot_escape_the_runs_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta("../../etc/x y")).unwrap();
        assert_eq!(
            dir.path(),
            tmp.path()
                .join("runs")
                .join("20261101T120000Z_rust-basics_..-..-etc-x-y")
        );
    }

    #[test]
    fn rewriting_meta_leaves_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap();
        let mut updated = meta("cheap");
        updated.finished_at = Some("2026-11-01T12:05:00Z".parse().unwrap());

        dir.write_meta(&updated).unwrap();

        assert_eq!(dir.read_meta().unwrap(), updated);
        assert!(!dir.path().join("meta.json.tmp").exists());
    }

    #[test]
    fn responses_are_appended_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap();

        let mut out = dir.appender().unwrap();
        out.append(&ok_line("a", 0)).unwrap();
        out.append(&ok_line("a", 1)).unwrap();
        drop(out);
        // 接續:另外打開一次,繼續往後加。
        dir.appender().unwrap().append(&ok_line("b", 0)).unwrap();

        let lines = dir.read_responses().unwrap();
        let keys: Vec<(&str, u32)> = lines
            .iter()
            .map(|l| (l.case_id.as_str(), l.sample))
            .collect();
        assert_eq!(keys, [("a", 0), ("a", 1), ("b", 0)]);
        assert_eq!(lines[0], ok_line("a", 0));
        assert_eq!(mode(&dir.path().join("responses.jsonl")), 0o600);
    }

    #[test]
    fn a_half_written_last_line_counts_as_not_done_and_is_sealed_off() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap();
        dir.appender().unwrap().append(&ok_line("a", 0)).unwrap();
        // 模擬當機:最後一行只寫了一半。
        let path = dir.path().join("responses.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"case_id":"b","sam"#).unwrap();

        // 讀的時候,那半行不算。
        let keys: Vec<String> = dir
            .read_responses()
            .unwrap()
            .into_iter()
            .map(|l| l.case_id)
            .collect();
        assert_eq!(keys, ["a"]);

        // 寫的時候先補換行,新的一行不會黏在半行後面。
        dir.appender().unwrap().append(&ok_line("c", 0)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let last = text.lines().last().unwrap();
        assert_eq!(
            serde_json::from_str::<ResponseLine>(last).unwrap(),
            ok_line("c", 0)
        );
    }

    #[test]
    fn other_read_errors_are_not_mistaken_for_no_results() {
        // 「還沒有結果」只限檔案不存在。讀不了(這裡是同名的目錄)要報錯,不能當成「全部沒做過」重跑一遍、再付一次錢。
        // (cargo-mutants:NotFound 的判斷換成 `true` 時沒有測試失敗。)
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap();
        std::fs::create_dir(dir.path().join("responses.jsonl")).unwrap();

        assert!(matches!(dir.read_responses(), Err(StoreError::Io { .. })));
    }

    #[test]
    fn a_broken_line_in_the_middle_is_an_error_with_its_line_number() {
        // 中間的行壞掉不是「寫到一半當機」,而是檔案被改壞了:不猜,直接報錯(fail-loud)。
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta("cheap")).unwrap();
        let path = dir.path().join("responses.jsonl");
        let good = serde_json::to_string(&ok_line("a", 0)).unwrap();
        std::fs::write(&path, format!("{good}\nnot json\n{good}\n")).unwrap();

        let err = dir.read_responses().unwrap_err();

        assert!(
            matches!(&err, StoreError::Corrupt { line: 2, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn response_lines_have_a_flat_json_shape() {
        let ok = serde_json::to_value(ok_line("a", 1)).unwrap();
        assert_eq!(ok["case_id"], "a");
        assert_eq!(ok["sample"], 1);
        assert_eq!(ok["outcome"], "ok");
        assert_eq!(ok["output"]["type"], "chat");
        assert_eq!(ok["cost_usd"], 0.5);

        let error = ResponseLine {
            case_id: "b".into(),
            sample: 0,
            result: CallResult::Error {
                error: RecordedError {
                    kind: "timeout".into(),
                    status: None,
                    message: "no answer within 300 s".into(),
                },
            },
            latency_ms: 300_000,
            cost_usd: None,
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({
                "case_id": "b",
                "sample": 0,
                "outcome": "error",
                "error": { "kind": "timeout", "message": "no answer within 300 s" },
                "latency_ms": 300_000
            })
        );
    }
}
