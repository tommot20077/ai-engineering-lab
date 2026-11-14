//! `read_file`、`list_dir`:只讀、限定在一個根目錄內(design §7、D-044)。
//!
//! 路徑是模型給的(不可信)。規則在 `Root::resolve` 一個地方:兩個工具共用,不會一個擋了一個沒擋。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolError};

/// `read_file` 讀得進來的檔案大小上限。
pub(crate) const MAX_FILE_BYTES: u64 = 256 * 1024;

/// `list_dir` 最多列幾項。
pub(crate) const MAX_ENTRIES: usize = 500;

/// 路徑字串的長度上限(bytes)。
const MAX_PATH_BYTES: usize = 1024;

/// 工具能讀的根目錄。建立時就 `canonicalize`(解開 symlink、變成絕對路徑),之後每次比對都用它。
#[derive(Debug, Clone)]
pub struct Root {
    canonical: PathBuf,
}

impl Root {
    /// 根目錄必須存在、而且是目錄(寫入路徑 fail-loud:設定錯了在啟動時就說)。
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Root> {
        let canonical = path.as_ref().canonicalize()?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        Ok(Root { canonical })
    }

    /// 把模型給的相對路徑換成根目錄底下、真實存在的絕對路徑。
    pub(crate) fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        let invalid = |reason: &str| ToolError::InvalidArgs(format!("path {path:?} {reason}"));
        if path.len() > MAX_PATH_BYTES {
            return Err(ToolError::InvalidArgs(format!(
                "path is longer than {MAX_PATH_BYTES} bytes"
            )));
        }
        // 第一關:只看字串的組成。`components()` 把路徑拆成一段一段,`..`、開頭的 `/` 各自是一種 Component。
        for component in Path::new(path).components() {
            match component {
                Component::Normal(name) => {
                    if is_hidden(name) {
                        return Err(invalid(
                            "is hidden (names starting with `.` are not readable)",
                        ));
                    }
                }
                Component::CurDir => {}
                Component::ParentDir => return Err(invalid("must not contain `..`")),
                // `/etc/passwd`、Windows 的 `C:\`:絕對路徑。
                Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid("must be relative to the allowed directory"));
                }
            }
        }
        // 第二關:看檔案系統。`canonicalize` 會解開每一層 symlink;
        // 結果必須還在根目錄底下,而且解開之後的每一段也不能是隱藏的(`notes -> .env` 也要擋)。
        let real = self
            .canonical
            .join(path)
            .canonicalize()
            .map_err(|_| ToolError::Failed(format!("no such file or directory: {path:?}")))?;
        // `strip_prefix` 比的是路徑的「段」,不是字串:`/data/root2` 不會被當成在 `/data/root` 底下。
        let inside = real
            .strip_prefix(&self.canonical)
            .map_err(|_| invalid("points outside the allowed directory"))?;
        if inside.components().any(|c| is_hidden(c.as_os_str())) {
            return Err(invalid("points to a hidden file"));
        }
        Ok(real)
    }
}

/// `.env`、`.git`:最常放秘密的地方(design §10-5)。
fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

// ---- read_file ----

/// `read_file` 工具。
pub struct ReadFile {
    root: Root,
}

impl ReadFile {
    pub fn new(root: Root) -> ReadFile {
        ReadFile { root }
    }
}

// 欄位上的 `///` 會送給模型看。
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    /// 檔案路徑,相對於允許的目錄,例如 "notes/todo.md"。
    pub path: String,
    /// 從第幾行開始(從 1 算)。省略時從第 1 行。
    // `range(min = 1)`:schema 寫上最小值 1,跟下面 `call` 拒絕 0 的規則一致(不然 schemars 從 u64 推出最小值 0)。
    #[schemars(range(min = 1))]
    pub offset: Option<u64>,
    /// 最多讀幾行。省略時讀到檔尾。
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
}

impl Tool for ReadFile {
    type Args = ReadFileArgs;

    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "讀取一個文字檔,每行前面加上行號。只能讀允許的目錄底下的檔案;不知道檔名時先用 list_dir。\
         檔案很長時用 offset 與 limit 分段讀。"
    }

    // 這裡用同步的檔案 API:檔案最大 256 KiB、在本機,讀取是微秒等級,不值得為它開執行緒。
    async fn call(&self, args: ReadFileArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        let offset = args.offset.unwrap_or(1);
        if offset == 0 {
            return Err(ToolError::InvalidArgs("offset starts at 1".into()));
        }
        if args.limit == Some(0) {
            return Err(ToolError::InvalidArgs("limit must be at least 1".into()));
        }
        let real = self.root.resolve(&args.path)?;
        let text = read_text(&real, &args.path)?;
        Ok(number_lines(&text, offset, args.limit))
    }
}

fn read_text(real: &Path, path: &str) -> Result<String, ToolError> {
    let failed = |reason: &str| ToolError::Failed(format!("{path:?} {reason}"));
    let file = std::fs::File::open(real).map_err(|_| failed("cannot be opened"))?;
    let metadata = file.metadata().map_err(|_| failed("cannot be opened"))?;
    if !metadata.is_file() {
        return Err(failed("is not a regular file"));
    }
    // 不信 metadata 的大小(檢查之後檔案可能變大):最多讀上限 + 1 byte,讀到超過就拒絕。
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failed("cannot be read"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(failed("is larger than 256 KiB"));
    }
    String::from_utf8(bytes).map_err(|_| failed("is not a UTF-8 text file"))
}

/// 加上行號;只選一部分時,最後註明是第幾行到第幾行。
fn number_lines(text: &str, offset: u64, limit: Option<u64>) -> String {
    let total = text.lines().count() as u64;
    if total == 0 {
        return "(empty file)".into();
    }
    if offset > total {
        return format!("(the file has only {total} lines)");
    }
    let last = match limit {
        Some(limit) => total.min(offset.saturating_add(limit) - 1),
        None => total,
    };
    let mut out = String::new();
    // `zip(1..)`:跟 1, 2, 3… 配對,得到行號。
    for (line, number) in text.lines().zip(1u64..) {
        if (offset..=last).contains(&number) {
            out.push_str(&format!("{number}\t{line}\n"));
        }
    }
    if offset > 1 || last < total {
        out.push_str(&format!("(lines {offset}-{last} of {total})"));
    } else {
        // 最後一行的換行拿掉,輸出比較乾淨。
        out.pop();
    }
    out
}

// ---- list_dir ----

/// `list_dir` 工具。
pub struct ListDir {
    root: Root,
}

impl ListDir {
    pub fn new(root: Root) -> ListDir {
        ListDir { root }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirArgs {
    /// 目錄路徑,相對於允許的目錄。省略時列出允許的目錄本身。
    pub path: Option<String>,
}

impl Tool for ListDir {
    type Args = ListDirArgs;

    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "列出目錄裡的檔案與子目錄(子目錄名稱後面有 /)。只能看允許的目錄底下;以 . 開頭的隱藏檔不會列出。"
    }

    async fn call(&self, args: ListDirArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        let path = args.path.as_deref().unwrap_or(".");
        let real = self.root.resolve(path)?;
        let failed = |reason: &str| ToolError::Failed(format!("{path:?} {reason}"));
        if !real.is_dir() {
            return Err(failed("is not a directory"));
        }
        let entries = std::fs::read_dir(&real).map_err(|_| failed("cannot be read"))?;
        let mut names: Vec<String> = entries
            // 讀不到的單一項目略過,不讓整個列表失敗。
            .filter_map(Result::ok)
            .filter(|entry| !is_hidden(&entry.file_name()))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                // `file_type()` 不跟著 symlink 走:指向目錄的 symlink 不會加 `/`。
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => format!("{name}/"),
                    _ => name,
                }
            })
            .collect();
        if names.is_empty() {
            return Ok("(empty directory)".into());
        }
        // `read_dir` 的順序由檔案系統決定;排序後每次結果一樣(模型、測試、prompt cache 都需要)。
        names.sort();
        let more = names.len().saturating_sub(MAX_ENTRIES);
        names.truncate(MAX_ENTRIES);
        let mut out = names.join("\n");
        if more > 0 {
            out.push_str(&format!("\n(and {more} more)"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
