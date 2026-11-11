//! 測試用的假時鐘(只在 `cargo test` 時編譯)。
//!
//! `run::start` / `run::resume` / `score::score` 不自己讀系統時間,而是收一個 `impl Fn() -> Timestamp`。
//! Java 對照:`Supplier<Instant>` / 注入 `java.time.Clock`。正式程式傳 `Timestamp::now`(函式本身,像 `Instant::now`),
//! 測試傳下面這些。

use jiff::{SignedDuration, Timestamp};

/// 永遠回傳同一個時間。不在乎開始與結束時間差多少的測試用。
pub(crate) fn fixed(at: &str) -> impl Fn() -> Timestamp {
    let at: Timestamp = at.parse().unwrap();
    move || at
}

/// 時間跟著「已經做了幾次呼叫」走:`start` 加上每次呼叫 1 分鐘。
///
/// 不用「每讀一次就前進一格」的時鐘:那樣的話,在工作開始前連讀兩次也會得到不同的值,
/// 測不出結束時間是不是「做完才讀」的。這個時鐘要等呼叫真的發生,時間才會前進。
pub(crate) fn minutes_per_call(start: &str, calls: impl Fn() -> usize) -> impl Fn() -> Timestamp {
    let start: Timestamp = start.parse().unwrap();
    move || start + SignedDuration::from_secs(60 * i64::try_from(calls()).unwrap())
}
