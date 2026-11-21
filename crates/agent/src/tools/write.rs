//! `write_file`、`edit_file`:有副作用的檔案工具(003 第三部分,D-058)。
//!
//! 只在 `--allow-write` 時註冊;每次呼叫之前都要核准(`needs_approval`),核准時看到的是**具體的變更**(`describe`)。
//! 路徑規則跟讀檔工具一樣在 `Root`;寫入一律是「暫存檔 + rename」(`Root::write`)。

use schemars::JsonSchema;
use serde::Deserialize;

use super::root::{MAX_FILE_BYTES, Root, Written};
use crate::tool::{Tool, ToolContext, ToolError};

/// 核准提示裡,每一段內容最多顯示幾行、幾個 bytes。
const PREVIEW_LINES: usize = 20;
const PREVIEW_BYTES: usize = 2000;

// ---- write_file ----

/// `write_file` 工具。
pub struct WriteFile {
    root: Root,
}

impl WriteFile {
    pub fn new(root: Root) -> WriteFile {
        WriteFile { root }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArgs {
    /// 檔案路徑,相對於允許的目錄,例如 "notes/todo.md"。上層目錄必須已經存在。
    pub path: String,
    /// 檔案的完整內容(整份取代原本的內容)。
    pub content: String,
}

impl Tool for WriteFile {
    type Args = WriteFileArgs;

    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "建立一個文字檔,或用新的內容整份覆寫已有的檔案。只能寫允許的目錄底下,上層目錄必須已經存在。\
         只改檔案裡的一小段時用 edit_file。每次寫入都要使用者核准,使用者可能拒絕。"
    }

    fn needs_approval(&self) -> bool {
        true
    }

    async fn describe(&self, args: &WriteFileArgs, _ctx: &ToolContext) -> Option<String> {
        let new = args.content.len();
        let head = match self.root.existing_size(&args.path) {
            Ok(None) => format!("create {} ({new} bytes)", args.path),
            Ok(Some(old)) => format!("overwrite {} ({old} bytes -> {new} bytes)", args.path),
            // 這次呼叫會失敗(路徑不合法、是目錄……):照實說,核准了也不會寫入任何東西。
            Err(e) => return Some(format!("{e} (nothing will be written)")),
        };
        Some(format!("{head}\n{}", preview(&args.content, "  ")))
    }

    async fn call(&self, args: WriteFileArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        let bytes = args.content.len();
        Ok(
            match self.root.write(&args.path, args.content.as_bytes())? {
                Written::Created => format!("created {} ({bytes} bytes)", args.path),
                Written::Overwritten => format!("overwrote {} ({bytes} bytes)", args.path),
            },
        )
    }
}

// ---- edit_file ----

/// `edit_file` 工具。
pub struct EditFile {
    root: Root,
}

impl EditFile {
    pub fn new(root: Root) -> EditFile {
        EditFile { root }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditFileArgs {
    /// 檔案路徑,相對於允許的目錄。
    pub path: String,
    /// 要被取代的文字:必須跟檔案內容一字不差(包括空白與換行),而且在檔案裡剛好出現一次。
    pub old: String,
    /// 換成這段文字。可以是空字串(等於刪掉 `old`)。
    pub new: String,
}

impl Tool for EditFile {
    type Args = EditFileArgs;

    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "把文字檔裡的一段文字換成另一段。old 必須跟檔案內容一字不差(包括空白與換行),而且在檔案裡剛好出現一次;\
         出現零次或不只一次都不會修改,先用 read_file 看清楚,或多包含一些前後文讓它只出現一次。\
         每次修改都要使用者核准,使用者可能拒絕。"
    }

    fn needs_approval(&self) -> bool {
        true
    }

    async fn describe(&self, args: &EditFileArgs, _ctx: &ToolContext) -> Option<String> {
        let planned = self
            .root
            .read_text(&args.path)
            .and_then(|text| plan(&text, args));
        Some(match planned {
            Ok(edit) => format!(
                "edit {} at line {}\n{}\n{}",
                args.path,
                edit.line,
                preview(&args.old, "- "),
                preview(&args.new, "+ ")
            ),
            Err(e) => format!("{e} (nothing will change)"),
        })
    }

    async fn call(&self, args: EditFileArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        // 核准之後重新讀一次、重新找:核准的時候看到的,可能已經被別人改掉了。找不到「剛好一次」就不改。
        let text = self.root.read_text(&args.path)?;
        let edit = plan(&text, &args)?;
        self.root.write(&args.path, edit.text.as_bytes())?;
        Ok(format!("edited {} at line {}", args.path, edit.line))
    }
}

/// 改好的內容與 `old` 開始的行號(從 1 算)。
#[derive(Debug, PartialEq, Eq)]
struct Edit {
    text: String,
    line: usize,
}

/// 在 `text` 裡找 `old`:剛好一次才取代。
fn plan(text: &str, args: &EditFileArgs) -> Result<Edit, ToolError> {
    let path = &args.path;
    // 第一個字元(下面找第二次出現時要跳過它);沒有第一個字元就是空字串。
    let Some(first) = args.old.chars().next() else {
        return Err(ToolError::InvalidArgs("`old` must not be empty".into()));
    };
    let Some(at) = text.find(&args.old) else {
        return Err(ToolError::Failed(format!(
            "`old` was not found in {path:?}; nothing was changed"
        )));
    };
    // 從下一個字元開始再找一次(允許重疊:"aa" 在 "aaa" 裡算出現兩次,取代哪一個都可能不是模型要的)。
    let next = at + first.len_utf8();
    if text
        .get(next..)
        .is_some_and(|rest| rest.contains(&args.old))
    {
        return Err(ToolError::Failed(format!(
            "`old` appears more than once in {path:?}; include more surrounding text so it appears \
             exactly once. Nothing was changed"
        )));
    }
    let new_text = format!(
        "{}{}{}",
        text.get(..at).unwrap_or_default(),
        args.new,
        text.get(at + args.old.len()..).unwrap_or_default()
    );
    if new_text.len() as u64 > MAX_FILE_BYTES {
        return Err(ToolError::Failed(format!(
            "{path:?} would be larger than 256 KiB; nothing was changed"
        )));
    }
    let line = text
        .get(..at)
        .map_or(1, |before| before.matches('\n').count() + 1);
    Ok(Edit {
        text: new_text,
        line,
    })
}

/// 核准提示用的預覽:每行前面加 `prefix`,最多 `PREVIEW_LINES` 行、`PREVIEW_BYTES` bytes,超過的註明。
fn preview(text: &str, prefix: &str) -> String {
    preview_capped(text, prefix, PREVIEW_LINES, PREVIEW_BYTES)
}

/// 同 `preview`,上限自己給(`run_rust` 要顯示比較長的程式)。
pub(crate) fn preview_capped(
    text: &str,
    prefix: &str,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    let total = text.lines().count();
    let mut out = Vec::new();
    let mut bytes = 0;
    for line in text.lines().take(max_lines) {
        let room = max_bytes.saturating_sub(bytes);
        if room == 0 {
            break;
        }
        let cut = line.floor_char_boundary(room);
        let shown = line.get(..cut).unwrap_or_default();
        bytes += shown.len();
        out.push(format!("{prefix}{shown}"));
    }
    if total == 0 {
        out.push(format!("{prefix}(empty)"));
    }
    let hidden = total.saturating_sub(out.len());
    if hidden > 0 || bytes >= max_bytes {
        out.push(format!("{prefix}... ({total} lines in total)"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests;
