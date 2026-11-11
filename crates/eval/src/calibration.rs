//! 校準集:量評審抓不抓得到錯、會不會冤枉對的(design-part2 §3、D-039)。
//!
//! 一個 JSONL 檔對應一個資料集(`evals/calibration/<資料集名稱>.jsonl`),一行一個「標好答案的回答」:
//! 拿已經確認正確的真實回答,只改一句讓它違反一條評分標準;沒改過的當對照。
//! 讀檔時檢查全部的行,有問題就拒絕整個檔案(寫入路徑 fail-loud,同資料集)。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::dataset::{line_of, without_position};
use crate::{Case, Dataset};

/// 讀進來、檢查過的校準集。
#[derive(Debug)]
pub struct CalibrationSet {
    /// 對應的資料集名稱。
    pub dataset: String,
    pub items: Vec<Item>,
}

/// 校準集的一行:一個回答,加上每一條評分標準「應該」得到的判決。
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    /// 回答的是資料集裡的哪一題。
    pub case_id: String,
    /// 這個回答原本是哪個模型寫的。用來看自我偏好:評審評自己寫的回答是不是比較寬鬆。
    pub source_model: String,
    pub kind: Kind,
    /// 改了什麼(給人看的)。對照組沒有。
    pub change: Option<String>,
    pub answer: String,
    /// 依案例評分標準的順序排好,一條標準一個標籤。
    pub labels: Vec<Label>,
}

/// 一條評分標準的標籤。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub criterion: String,
    pub expected: Expected,
}

/// 回答是怎麼來的(design-part2 §3)。報告依它分組:明顯的錯抓得到、微妙的抓不到,是很常見的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// 沒改過的正確回答:量評審會不會冤枉。
    Control,
    Obvious,
    Subtle,
    Fabricated,
    OffTopic,
    /// 改壞一句,再加一句要評審判通過的話:量 D-031 的防禦。
    Injection,
}

/// 一條標準應該得到的判決。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expected {
    Pass,
    Fail,
}

#[derive(Debug, thiserror::Error)]
pub enum CalibrationError {
    // 訊息裡不放 `{source}`:原因由 `#[source]` 交給錯誤鏈印(anyhow 會印「Caused by: …」),
    // 兩邊都放就會印兩次(ROADMAP 待辦記的那個毛病,新的錯誤型別一開始就避開)。
    #[error("cannot read calibration set {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// 一次列出**全部**的問題,每一條都帶行號。
    #[error(
        "invalid calibration set for dataset `{dataset}`:\n{}",
        bullet_list(problems)
    )]
    Invalid {
        dataset: String,
        problems: Vec<String>,
    },
}

fn bullet_list(problems: &[String]) -> String {
    problems
        .iter()
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl CalibrationSet {
    /// 讀一個校準集檔案,對照它的資料集檢查。
    pub fn load(path: &Path, dataset: &Dataset) -> Result<Self, CalibrationError> {
        let bytes = std::fs::read(path).map_err(|source| CalibrationError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&bytes, dataset)
    }

    /// 解析校準集的內容。純函式:不碰檔案系統,測試直接餵字串。
    pub fn parse(bytes: &[u8], dataset: &Dataset) -> Result<Self, CalibrationError> {
        let invalid = |problems| CalibrationError::Invalid {
            dataset: dataset.name.clone(),
            problems,
        };
        let text = std::str::from_utf8(bytes).map_err(|e| {
            let line = line_of(bytes, e.valid_up_to());
            invalid(vec![format!("line {line}: not valid UTF-8")])
        })?;
        // 案例 id → 案例。借用資料集裡的資料,不複製:`HashMap<&str, &Case>` 活得不會比 `dataset` 久。
        let cases: HashMap<&str, &Case> =
            dataset.cases.iter().map(|c| (c.id.as_str(), c)).collect();

        let mut problems = Vec::new();
        let mut items = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (index, text_line) in text.lines().enumerate() {
            let line = index + 1;
            if text_line.trim().is_empty() {
                continue;
            }
            let raw: RawItem = match serde_json::from_str(text_line) {
                Ok(raw) => raw,
                Err(e) => {
                    problems.push(format!(
                        "line {line}, column {}: {}",
                        e.column(),
                        without_position(&e)
                    ));
                    continue;
                }
            };
            if !raw.id.is_empty() {
                match seen.entry(raw.id.clone()) {
                    Entry::Occupied(first) => problems.push(format!(
                        "line {line}: duplicate id `{}` (first used on line {})",
                        raw.id,
                        first.get()
                    )),
                    Entry::Vacant(slot) => {
                        slot.insert(line);
                    }
                }
            }
            let at = match raw.id.as_str() {
                "" => format!("line {line}"),
                id => format!("line {line} (`{id}`)"),
            };
            match raw.validate(&cases, &dataset.name) {
                Ok(item) => items.push(item),
                Err(found) => problems.extend(found.into_iter().map(|p| format!("{at}: {p}"))),
            }
        }
        if items.is_empty() && problems.is_empty() {
            problems.push("the calibration set has no answers".into());
        }
        if !problems.is_empty() {
            return Err(invalid(problems));
        }
        Ok(Self {
            dataset: dataset.name.clone(),
            items,
        })
    }
}

/// 檔案裡的一行,serde 直接讀出來、還沒檢查。檢查過才變成 `Item`:
/// 程式其他地方拿到的 `Item` 一定是合法的,不必每次用之前再檢查一遍(parse, don't validate)。
/// Java 對照:DTO 只有在工廠方法驗證通過後才轉成 domain 物件。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawItem {
    id: String,
    case: String,
    source_model: String,
    kind: Kind,
    #[serde(default)]
    change: Option<String>,
    answer: String,
    #[serde(deserialize_with = "unique_entries")]
    expected: Vec<(String, Expected)>,
}

impl RawItem {
    /// 收集這一行的全部問題;有任何一個就不產生 `Item`。
    // `self`(不是 `&self`):檢查完把欄位直接搬進 `Item`,不必複製字串。檢查失敗時 `self` 就丟掉了。
    fn validate(self, cases: &HashMap<&str, &Case>, dataset: &str) -> Result<Item, Vec<String>> {
        let mut problems = Vec::new();
        for (field, value) in [
            ("id", &self.id),
            ("answer", &self.answer),
            ("source_model", &self.source_model),
        ] {
            if value.trim().is_empty() {
                problems.push(format!("`{field}` is empty"));
            }
        }

        let labels = match cases.get(self.case.as_str()) {
            None => {
                problems.push(format!(
                    "unknown case `{}` in dataset `{dataset}`",
                    self.case
                ));
                Vec::new()
            }
            Some(case) if case.rubric.is_empty() => {
                problems.push(format!(
                    "case `{}` has no rubric, so there is nothing for a judge to calibrate",
                    self.case
                ));
                Vec::new()
            }
            Some(case) => {
                for (criterion, _) in &self.expected {
                    if !case.rubric.contains(criterion) {
                        problems.push(format!(
                            "`expected` names a criterion that case `{}` does not have: {criterion:?}",
                            self.case
                        ));
                    }
                }
                // 依評分標準的順序排:之後跟評審的判決逐條對照。
                let mut labels = Vec::new();
                for criterion in &case.rubric {
                    match self.expected.iter().find(|(c, _)| c == criterion) {
                        Some(&(_, expected)) => labels.push(Label {
                            criterion: criterion.clone(),
                            expected,
                        }),
                        None => problems
                            .push(format!("`expected` is missing the criterion {criterion:?}")),
                    }
                }
                labels
            }
        };

        let expects_a_failure = self.expected.iter().any(|&(_, e)| e == Expected::Fail);
        let says_what_changed = self.change.as_deref().is_some_and(|c| !c.trim().is_empty());
        if self.kind == Kind::Control {
            if expects_a_failure {
                problems.push("a control answer must expect every criterion to pass".into());
            }
            if self.change.is_some() {
                problems.push("a control answer was not changed, so it has no `change`".into());
            }
        } else {
            if !expects_a_failure {
                problems.push("a changed answer must expect at least one criterion to fail".into());
            }
            if !says_what_changed {
                problems.push("`change` must say what was changed".into());
            }
        }

        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(Item {
            id: self.id,
            case_id: self.case,
            source_model: self.source_model,
            kind: self.kind,
            change: self.change,
            answer: self.answer,
            labels,
        })
    }
}

/// 把 `expected` 物件讀成 (標準, 標籤) 的清單,同一條標準出現兩次就報錯。
///
/// serde 讀成 `HashMap` 時,重複的 key 會被後面的默默蓋掉:同一條標準寫了 `pass` 又寫 `fail`,
/// 只剩最後一個,沒有人會發現。所以自己寫 visitor:serde 每讀到一個 key / value 就交給我們,一個一個收。
/// Java 對照:Jackson 要打開 `JsonParser.Feature.STRICT_DUPLICATE_DETECTION` 才會擋重複的 key。
fn unique_entries<'de, D>(deserializer: D) -> Result<Vec<(String, Expected)>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Entries;

    impl<'de> Visitor<'de> for Entries {
        type Value = Vec<(String, Expected)>;

        // 型別不對時(例如給了陣列),錯誤訊息裡的「expected …」就是這句。
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an object mapping each criterion to \"pass\" or \"fail\"")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries: Vec<(String, Expected)> = Vec::new();
            while let Some((criterion, expected)) = map.next_entry::<String, Expected>()? {
                if entries.iter().any(|(seen, _)| *seen == criterion) {
                    return Err(de::Error::custom(format!(
                        "criterion {criterion:?} is listed twice"
                    )));
                }
                entries.push((criterion, expected));
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries)
}

#[cfg(test)]
mod tests;
