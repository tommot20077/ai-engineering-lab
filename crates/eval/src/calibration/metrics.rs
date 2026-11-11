//! 校準的指標(design-part2 §4):抓錯率、冤枉率、評審之間的一致率,每個比例附 Wilson 95% 區間。
//!
//! 全部是純函式:輸入是「標籤說應該怎樣、評審說怎樣」的配對,不碰檔案、不呼叫模型。
//! 依種類、依來源模型分組,由呼叫的人先篩選再交給 `Tally`。

use super::Expected;
use crate::score::VerdictResult;

/// 一個比例(`k / n`)和它的 Wilson 95% 信賴區間。比例與區間都是 0 到 1。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Proportion {
    pub k: usize,
    pub n: usize,
    pub rate: f64,
    pub low: f64,
    pub high: f64,
}

/// 標準常態分布的 97.5% 分位數:95% 雙尾區間的 z。等於 Python 的 `NormalDist().inv_cdf(0.975)`。
const Z95: f64 = 1.959_963_984_540_053_6;

impl Proportion {
    /// `n` 是 0 時回傳 `None`:沒有證據,不是 0%,也不是 100%。
    ///
    /// 區間用 Wilson score interval:樣本小、比例接近 0 或 1 時(校準最常見的情況:評審幾乎全抓到),
    /// 常見的「p ± 1.96·√(p(1−p)/n)」會給出寬度 0 或超出 [0, 1] 的區間,Wilson 不會。
    pub fn new(k: usize, n: usize) -> Option<Self> {
        if n == 0 {
            return None;
        }
        // `as f64`:整數轉浮點數。計數最多幾千,轉換是精確的。Java 對照:`(double) k`。
        let (kf, nf) = (k as f64, n as f64);
        let rate = kf / nf;
        let z2 = Z95 * Z95;
        let denominator = 1.0 + z2 / nf;
        let center = (rate + z2 / (2.0 * nf)) / denominator;
        let half = Z95 * (rate * (1.0 - rate) / nf + z2 / (4.0 * nf * nf)).sqrt() / denominator;
        // Wilson 區間一定包含觀察到的比例,而且在 [0, 1] 裡;但浮點誤差會讓 0/5 的下界算成 2.8e-17(比 0 還大)、
        // 92/92 的上界算成 1.0000000000000002。所以下界夾在 [0, rate]、上界夾在 [rate, 1]。
        // `clamp` 在 min > max 時會 panic;`rate` 一定在 [0, 1] 裡(n > 0),不會發生。
        Some(Self {
            k,
            n,
            rate,
            low: (center - half).clamp(0.0, rate),
            high: (center + half).clamp(rate, 1.0),
        })
    }
}

/// 一組判決的計數。`Tally` 可以直接 `collect()` 出來(見 `FromIterator`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    /// 標成 `fail` 的標準。
    pub expected_fail: usize,
    /// 其中評審判 `fail` 的:抓到了。
    pub caught: usize,
    /// 其中評審判 `pass` 的:漏掉了(`injection` 的回答漏掉,就是被騙)。
    pub passed_through: usize,
    /// 標成 `pass` 的標準。
    pub expected_pass: usize,
    /// 其中評審判 `fail` 的:冤枉。
    pub false_fail: usize,
    /// 評審沒有給出判決(`Error`;`Skipped` 在校準時不應該出現,一樣算在這裡)。
    pub errors: usize,
}

impl Tally {
    /// 抓錯率:標成 `fail` 的標準裡,評審判 `fail` 的比例。
    pub fn catch_rate(&self) -> Option<Proportion> {
        // 評審出錯的那幾條也在分母裡:沒說不通過,就不能算它抓到(fail-closed)。
        Proportion::new(self.caught, self.expected_fail)
    }

    /// 冤枉率:標成 `pass` 的標準裡,評審判 `fail` 的比例。
    pub fn false_fail_rate(&self) -> Option<Proportion> {
        Proportion::new(self.false_fail, self.expected_pass)
    }
}

/// 讓 `Tally` 可以用 `collect()` 從(標籤, 判決)的配對算出來。
/// Java 對照:自己寫一個 `Collector`,再 `stream.collect(myCollector)`。
impl FromIterator<(Expected, VerdictResult)> for Tally {
    fn from_iter<I: IntoIterator<Item = (Expected, VerdictResult)>>(iter: I) -> Self {
        let mut tally = Tally::default();
        for (expected, verdict) in iter {
            match expected {
                Expected::Fail => tally.expected_fail += 1,
                Expected::Pass => tally.expected_pass += 1,
            }
            // 一個 match 列出全部 8 種組合:之後 `VerdictResult` 多一種,compiler 會指出這裡要決定怎麼算。
            match (expected, verdict) {
                (Expected::Fail, VerdictResult::Fail) => tally.caught += 1,
                (Expected::Fail, VerdictResult::Pass) => tally.passed_through += 1,
                (Expected::Pass, VerdictResult::Fail) => tally.false_fail += 1,
                (Expected::Pass, VerdictResult::Pass) => {}
                (_, VerdictResult::Error | VerdictResult::Skipped) => tally.errors += 1,
            }
        }
        tally
    }
}

/// 兩個評審對同一批標準的一致率。只算兩邊都給出判決(`pass` 或 `fail`)的標準。
pub fn agreement(
    pairs: impl IntoIterator<Item = (VerdictResult, VerdictResult)>,
) -> Option<Proportion> {
    let decided = |v: VerdictResult| matches!(v, VerdictResult::Pass | VerdictResult::Fail);
    let (mut same, mut both) = (0, 0);
    for (a, b) in pairs {
        if decided(a) && decided(b) {
            both += 1;
            if a == b {
                same += 1;
            }
        }
    }
    Proportion::new(same, both)
}

#[cfg(test)]
mod tests;
