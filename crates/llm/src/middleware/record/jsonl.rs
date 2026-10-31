//! 把紀錄寫成 JSONL 檔:每天(UTC)一個檔,一筆一行(design-part2.md §15、D-021)。
//!
//! ```text
//! 請求 1 的 Record ─┐
//! 請求 2 的 Record ─┼─→ [ 有上限的佇列 ] ─→ 寫檔執行緒(只有一條)─→ 2026-10-31.jsonl
//! 請求 3 的 Record ─┘
//! ```
//!
//! 只支援 Unix(macOS、Linux):目錄與檔案的權限要在建立的那一刻就設好。

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

use super::{CallRecord, RecordSink};

/// 佇列的預設上限。硬碟出問題時,最多在記憶體裡堆這麼多筆,不會無限長大。
pub const DEFAULT_QUEUE_CAPACITY: usize = 1000;

/// 背景寫檔的 `RecordSink`。
///
/// 被丟掉(`Drop`)或呼叫 `shutdown` 時,會先把佇列裡剩下的紀錄寫完,再結束執行緒。
pub struct JsonlWriter {
    // `Option`:關閉時要先把 sender 丟掉,寫檔執行緒才知道「不會再有新的了」。
    sender: Option<SyncSender<CallRecord>>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl JsonlWriter {
    /// 檢查目錄、開始背景寫檔。目錄不存在就以 700 建立;存在但其他人讀得到就拒絕。
    ///
    /// 在啟動時呼叫:設定有錯就在這裡失敗,而不是等到第一筆紀錄才發現(D-021 (3))。
    pub fn start(dir: impl Into<PathBuf>, capacity: usize) -> io::Result<Self> {
        let dir = dir.into();
        prepare_dir(&dir)?;
        // 有上限的佇列:滿了 `try_send` 會立刻失敗,不會讓送紀錄的一方等待(D-021 (4))。
        // Java 對照:`new ArrayBlockingQueue<>(capacity)` 搭配 `offer()`(不是會卡住的 `put()`)。
        let (sender, receiver) = sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        // 一條專門寫檔的 OS 執行緒。寫檔是會卡住的 I/O,不能放在 tokio 處理請求的執行緒上。
        // Java 對照:`Executors.newSingleThreadExecutor()`,或 Logback 的 `AsyncAppender`。
        let thread = std::thread::Builder::new()
            .name("call-record-writer".into())
            .spawn({
                // 執行緒要自己持有一份計數器;`move` 把 `dir`、`receiver`、這份 `dropped` 都搬進去。
                let dropped = Arc::clone(&dropped);
                move || run(&dir, receiver, &dropped)
            })?;
        Ok(Self::from_parts(sender, dropped, Some(thread)))
    }

    fn from_parts(
        sender: SyncSender<CallRecord>,
        dropped: Arc<AtomicU64>,
        thread: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            sender: Some(sender),
            dropped,
            thread,
        }
    }

    /// 到目前為止遺失了幾筆(佇列滿、或寫入失敗)。gateway 的 `/v1/status` 會顯示它。
    pub fn dropped(&self) -> u64 {
        // `Relaxed`:這只是一個計數器,不用來協調其他資料,不需要更強的記憶體順序保證。
        // Java 對照:`AtomicLong.get()`。
        self.dropped.load(Ordering::Relaxed)
    }

    /// 寫完佇列裡剩下的紀錄,然後結束。等於明確地 `drop`,實際的工作在 `Drop` 裡。
    pub fn shutdown(self) {
        drop(self);
    }
}

impl RecordSink for JsonlWriter {
    fn submit(&self, record: CallRecord) {
        let Some(sender) = &self.sender else {
            return;
        };
        // 不等待:佇列滿了(硬碟太慢或壞了)就丟掉這一筆,並且大聲報錯(D-021 (3))。
        // `Full(record) | Disconnected(record)`:兩種失敗都把紀錄還給我們,用 `|` 一起處理。
        if let Err(TrySendError::Full(record) | TrySendError::Disconnected(record)) =
            sender.try_send(record)
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::error!(id = %record.id, "call record dropped: writer queue is full or stopped");
        }
    }
}

impl Drop for JsonlWriter {
    fn drop(&mut self) {
        // 先丟掉 sender:寫檔執行緒的 `for record in receiver` 讀完剩下的就會結束。
        drop(self.sender.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            // join 失敗 = 寫檔執行緒 panic 了。已經在關機,記下來就好。
            tracing::error!("call record writer thread panicked");
        }
    }
}

/// 建立目錄(700),或確認既有目錄只有自己能進入,並確認可以寫入。
fn prepare_dir(dir: &Path) -> io::Result<()> {
    // `mode(0o700)`:在「建立的那一刻」就設好權限,不是先建立再 chmod(中間會有一段空窗)。
    // `recursive(true)`:上層不存在也一起建立;目錄已經存在不算錯誤。
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "records directory {} is accessible by other users (mode {mode:o}); run `chmod 700` on it",
                dir.display()
            ),
        ));
    }
    // 真的寫一次看看:權限位元看起來對,不代表寫得進去(例如唯讀的磁碟)。
    let probe = dir.join(".write-check");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

/// 目前開著的檔案,以及它是哪一天的。
struct DayFile {
    day: String,
    file: File,
}

/// 寫檔執行緒的主迴圈。
fn run(dir: &Path, receiver: Receiver<CallRecord>, dropped: &AtomicU64) {
    let mut current: Option<DayFile> = None;
    // 沒有紀錄時在這裡等;所有 sender 都被丟掉、而且佇列讀空之後,迴圈自然結束。
    // 所以「關機前寫完剩下的」不用另外寫程式,這就是 channel 的語意。
    for record in receiver {
        if let Err(error) = append(dir, &mut current, &record) {
            dropped.fetch_add(1, Ordering::Relaxed);
            tracing::error!(%error, id = %record.id, "failed to write call record");
            // 下次重新開檔,順便把可能寫了一半的那一行封起來。
            current = None;
        }
    }
    // 正常關機:最後開著的檔案也要真的寫進磁碟。
    if let Some(open) = current {
        sync(open);
    }
}

/// 把作業系統快取裡的資料真的寫進磁碟,然後關檔。
///
/// `write_all` 只把資料交給作業系統,過幾秒才會落地:程式當掉不會遺失,但整台電腦斷電可能遺失最後幾秒。
/// 每筆都 sync 太慢(一次可能幾毫秒),所以只在換日與關機時各做一次(D-021)。
// 參數是 `DayFile`(不是 `&DayFile`):sync 完這個檔案就交出去、函式結束時被丟掉(自動關檔)。
fn sync(open: DayFile) {
    // `sync_data`:只保證內容落地,不等檔案的修改時間等中繼資料,比 `sync_all` 快一點。
    // Java 對照:`FileChannel.force(false)`。
    if let Err(error) = open.file.sync_data() {
        tracing::error!(%error, day = %open.day, "failed to sync call records to disk");
    }
}

/// 把一筆紀錄寫成一行。換日時換檔。
fn append(dir: &Path, current: &mut Option<DayFile>, record: &CallRecord) -> io::Result<()> {
    // 用紀錄自己的 started_at(UTC)決定檔名,不用寫入當下的時間:跨日可以直接測試。
    let day = record.started_at.strftime("%Y-%m-%d").to_string();
    // `take()` 先把目前的檔案拿出來:同一天就放回去,不同天就 sync 後關掉、開新的。
    // `insert` 放回去並回傳裡面的 `&mut`,所以不需要再「確認它一定是 Some」。
    let open = match current.take() {
        Some(open) if open.day == day => current.insert(open),
        previous => {
            // 換日(或第一次開檔、或上次寫入失敗後重開):前一天的檔案先確實落地再關。
            if let Some(previous) = previous {
                sync(previous);
            }
            let file = open_day_file(&dir.join(format!("{day}.jsonl")))?;
            current.insert(DayFile { day, file })
        }
    };
    // 先在記憶體裡組好完整的一行(含換行),再一次寫出去(D-021 (3))。
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    open.file.write_all(&line)
}

/// 開啟(或建立,600)某一天的檔案。
fn open_day_file(path: &Path) -> io::Result<File> {
    // `append(true)`:每次寫入都接在檔尾。`read(true)`:要讀最後一個字元(下面的 seal)。
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    seal_partial_line(&mut file)?;
    Ok(file)
}

/// 上次當機可能留下寫了一半的行。如果最後一個字元不是換行,先補一個,
/// 否則下一筆完整的紀錄會黏在半行後面,一起壞掉(「不要污染已存的資料」)。
fn seal_partial_line(file: &mut File) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    // `b"\n"` 是長度 1 的 byte 陣列的參考;`*` 取出陣列本身,才能跟 `last` 比較。
    if last != *b"\n" {
        file.write_all(b"\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::record::{Latency, Outcome};
    use crate::{ProviderMetadata, Request, Usage};

    fn record(started_at: &str, model: &str) -> CallRecord {
        CallRecord {
            schema_version: 1,
            id: uuid::Uuid::now_v7(),
            started_at: started_at.parse().unwrap(),
            requested_model: model.into(),
            served_model: None,
            request: Request {
                model: model.into(),
                system: None,
                messages: vec![],
                tools: vec![],
                max_tokens: 1,
                provider_options: ProviderMetadata::default(),
            },
            response: None,
            outcome: Outcome::Ok,
            error: None,
            usage: Usage::default(),
            latency_ms: Latency {
                first_output: None,
                total: 1,
            },
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn each_record_becomes_one_line_in_the_file_of_its_utc_day() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("records");
        let late = record("2026-10-30T23:59:59Z", "a");
        let early = record("2026-10-31T00:00:01Z", "b");

        let writer = JsonlWriter::start(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        writer.submit(late.clone());
        writer.submit(early.clone());
        writer.shutdown();

        // 檔名看的是紀錄的 started_at(UTC),不是寫入當下的時間:跨日可以這樣直接測。
        let day1 = lines(&dir.join("2026-10-30.jsonl"));
        let day2 = lines(&dir.join("2026-10-31.jsonl"));
        assert_eq!(day1.len(), 1);
        assert_eq!(day2.len(), 1);
        assert_eq!(serde_json::from_str::<CallRecord>(&day1[0]).unwrap(), late);
        assert_eq!(serde_json::from_str::<CallRecord>(&day2[0]).unwrap(), early);
    }

    #[test]
    fn directory_is_created_700_and_files_600() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("records");

        let writer = JsonlWriter::start(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        writer.submit(record("2026-10-31T00:00:00Z", "m"));
        writer.shutdown();

        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("2026-10-31.jsonl")), 0o600);
    }

    #[test]
    fn existing_directory_readable_by_others_is_rejected_at_start() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("records");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = JsonlWriter::start(&dir, DEFAULT_QUEUE_CAPACITY)
            .err()
            .expect("應該拒絕啟動");

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("755"), "{err}");
    }

    #[test]
    fn shutdown_writes_everything_still_in_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = JsonlWriter::start(tmp.path().join("r"), DEFAULT_QUEUE_CAPACITY).unwrap();
        for _ in 0..200 {
            writer.submit(record("2026-10-31T12:00:00Z", "m"));
        }
        writer.shutdown();

        assert_eq!(
            lines(&tmp.path().join("r").join("2026-10-31.jsonl")).len(),
            200
        );
    }

    #[test]
    fn dropping_the_writer_also_flushes_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = JsonlWriter::start(tmp.path().join("r"), DEFAULT_QUEUE_CAPACITY).unwrap();
        writer.submit(record("2026-10-31T12:00:00Z", "m"));
        drop(writer);

        assert_eq!(
            lines(&tmp.path().join("r").join("2026-10-31.jsonl")).len(),
            1
        );
    }

    #[test]
    fn a_full_queue_drops_the_record_and_counts_it() {
        // 自己拿著 receiver、但不讀:模擬寫檔執行緒卡住(例如硬碟很慢)。
        let (sender, _receiver) = sync_channel(1);
        // `_receiver`(有名字)會活到測試結束;如果寫成 `let (sender, _)`,receiver 會立刻被丟掉,
        // 變成「寫檔執行緒已停止」,測到的就是另一種失敗了。
        let writer = JsonlWriter::from_parts(sender, Arc::default(), None);

        writer.submit(record("2026-10-31T12:00:00Z", "m")); // 佔滿唯一的位置
        writer.submit(record("2026-10-31T12:00:00Z", "m")); // 滿了 → 丟棄

        assert_eq!(writer.dropped(), 1);
    }

    #[test]
    fn a_failed_write_is_counted_and_the_writer_keeps_going() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("r");
        let writer = JsonlWriter::start(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        // 讓某一天的檔名被一個目錄佔住:那天的紀錄一定寫不進去。
        std::fs::create_dir(dir.join("2026-10-30.jsonl")).unwrap();

        writer.submit(record("2026-10-30T12:00:00Z", "lost"));
        writer.submit(record("2026-10-31T12:00:00Z", "kept"));
        let dropped = Arc::clone(&writer.dropped);
        writer.shutdown();

        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(
            lines(&dir.join("2026-10-31.jsonl")).len(),
            1,
            "壞掉一筆不影響後面的"
        );
    }

    #[test]
    fn a_half_written_last_line_is_sealed_off_before_appending() {
        // 上次當機留下半行。直接接在後面寫,下一筆完整的紀錄也會跟著壞掉。
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("r");
        let writer = JsonlWriter::start(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        let path = dir.join("2026-10-31.jsonl");
        std::fs::write(&path, "{\"schema_version\":1,\"id\":\"half").unwrap();

        let fresh = record("2026-10-31T12:00:00Z", "m");
        writer.submit(fresh.clone());
        writer.shutdown();

        let lines = lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<CallRecord>(&lines[1]).unwrap(),
            fresh
        );
    }
}
