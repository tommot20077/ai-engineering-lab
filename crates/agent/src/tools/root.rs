//! `Root`:檔案工具能碰的根目錄(design §7、D-044;第三部分改用 cap-std,D-055)。
//!
//! 路徑是模型給的(不可信)。規則全部在這裡,四個檔案工具共用:
//!
//! 1. **字串**:不能是絕對路徑、不能有 `..`、不能有以 `.` 開頭的段(`.env`、`.git`)、最長 1024 bytes。
//! 2. **檔案系統**:從根目錄的 handle(一個已經開好的目錄)**一段一段往下開**,每一段都不跟隨 symlink。
//!
//! 為什麼不是「拼成絕對路徑、canonicalize、檢查前綴、再開檔」(第一部分的做法):檢查與開檔之間有空隙(TOCTOU)——
//! 檢查完、開檔前,某一段被換成指向外面的 symlink,開到的就是外面的檔案。讀檔時這是洩漏,寫檔時不能復原。
//! 用 handle 開檔(底下是 `openat`),每一步都是「在這個已經開好的目錄裡、開這個名字」,沒有「之後再照路徑找一次」。
//! Java 對照:`Path.resolve()` + `toRealPath()` + `startsWith()` 就是第一部分那種做法;Java 標準庫沒有 handle 版本的 API。

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};

use crate::tool::ToolError;

/// 檔案大小上限:`read_file` 讀得進來的、`write_file` / `edit_file` 寫得出去的。
pub(crate) const MAX_FILE_BYTES: u64 = 256 * 1024;

/// 路徑字串的長度上限(bytes)。
const MAX_PATH_BYTES: usize = 1024;

/// 工具能碰的根目錄。
///
/// `Arc<Dir>`:四個工具各拿一份 `Root`,共用同一個目錄 handle(`Dir` 本身不能 `Clone`:複製 handle 要 `dup`,可能失敗)。
#[derive(Debug, Clone)]
pub struct Root {
    dir: Arc<Dir>,
    /// 只用來顯示(錯誤訊息、核准提示),不用來開檔。
    path: PathBuf,
}

/// 寫入的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    Created,
    Overwritten,
}

impl Root {
    /// 根目錄必須存在、而且是目錄(寫入路徑 fail-loud:設定錯了在啟動時就說)。
    /// 這是唯一一個用「路徑」開東西的地方(`ambient_authority()` 就是在標示這件事);之後全部從這個 handle 往下開。
    pub fn new(path: impl AsRef<Path>) -> io::Result<Root> {
        let path = path.as_ref().canonicalize()?;
        if !path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            ));
        }
        let dir = Dir::open_ambient_dir(&path, ambient_authority())?;
        Ok(Root {
            dir: Arc::new(dir),
            path,
        })
    }

    /// 根目錄的絕對路徑(顯示用)。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 開一個目錄(`list_dir`)。`"."` 是根目錄本身。
    pub(crate) fn open_dir(&self, path: &str) -> Result<Dir, ToolError> {
        let segments = segments(path)?;
        self.walk(path, &segments)
    }

    /// 讀一個 UTF-8 文字檔,最多 `MAX_FILE_BYTES`。
    pub(crate) fn read_text(&self, path: &str) -> Result<String, ToolError> {
        let failed = |reason: &str| ToolError::Failed(format!("{path:?} {reason}"));
        let (dir, name) = self.parent(path)?;
        // 先確認是一般檔案:FIFO 之類的「檔案」開起來會卡住。真正擋 symlink 的是下面的 `FollowSymlinks::No`。
        match dir.symlink_metadata(&name) {
            Ok(meta) if meta.is_symlink() => return Err(symlink(path)),
            Ok(meta) if !meta.is_file() => return Err(failed("is not a regular file")),
            Ok(_) => {}
            Err(_) => return Err(not_found(path)),
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = dir
            .open_with(&name, &options)
            .map_err(|_| failed("cannot be opened"))?;
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

    /// 這個路徑現在是什麼:不存在、一般檔案(大小),或不能寫的東西(錯誤)。給核准提示用。
    pub(crate) fn existing_size(&self, path: &str) -> Result<Option<u64>, ToolError> {
        let (dir, name) = self.parent(path)?;
        Ok(target(&dir, &name, path)?.map(|meta| meta.len()))
    }

    /// 寫入整個檔案:先寫到同一個目錄裡的暫存檔、`sync_all`、再 `rename` 蓋過去。
    ///
    /// 讀的人只會看到舊的或新的內容,不會看到寫到一半的檔案;寫到一半失敗(磁碟滿),原本的檔案不受影響。
    /// 上層目錄必須已經存在(不幫模型建目錄)。覆寫時保留原本的權限。
    pub(crate) fn write(&self, path: &str, bytes: &[u8]) -> Result<Written, ToolError> {
        let failed = |reason: &str| ToolError::Failed(format!("{path:?} {reason}"));
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(failed("would be larger than 256 KiB"));
        }
        let (dir, name) = self.parent(path)?;
        let existing = target(&dir, &name, path)?;

        // 暫存檔的名字以 `.` 開頭:檔案工具看不到它(隱藏檔的規則),中途當掉留下來也不會被模型讀到。
        let mut temp = OsString::from(".");
        temp.push(&name);
        temp.push(format!(".{}.tmp", uuid::Uuid::now_v7().simple()));
        let mut options = OpenOptions::new();
        // `create_new`:同名的已經存在就失敗(不會寫進別人的檔案,也不會跟著 symlink 走)。
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let file = dir
            .open_with(&temp, &options)
            .map_err(|_| failed("cannot be written"))?;
        let filled = fill(
            file,
            bytes,
            existing.as_ref().map(|meta| meta.permissions()),
        );
        // `rename` 蓋掉目標:目標如果在這之間被換成 symlink,換掉的是 symlink 本身,不會寫到它指的地方。
        let renamed = filled.and_then(|()| dir.rename(&temp, &dir, &name));
        if renamed.is_err() {
            let _ = dir.remove_file(&temp);
            return Err(failed("cannot be written"));
        }
        Ok(if existing.is_some() {
            Written::Overwritten
        } else {
            Written::Created
        })
    }

    /// 開路徑的上一層目錄,回傳它與最後一段的名字。路徑至少要有一段(不能是根目錄本身)。
    fn parent(&self, path: &str) -> Result<(Dir, OsString), ToolError> {
        let segments = segments(path)?;
        let Some((last, parents)) = segments.split_last() else {
            return Err(ToolError::InvalidArgs(format!(
                "path {path:?} must name a file"
            )));
        };
        Ok((self.walk(path, parents)?, (*last).to_owned()))
    }

    /// 從根目錄往下開每一段;每一段都必須是真的目錄(不是 symlink)。
    fn walk(&self, path: &str, segments: &[&OsStr]) -> Result<Dir, ToolError> {
        let mut dir = self
            .dir
            .try_clone()
            .map_err(|_| ToolError::Failed("the allowed directory cannot be opened".into()))?;
        for segment in segments {
            // `open_dir_nofollow`:這一段是 symlink 就失敗(O_NOFOLLOW),不會被帶到別的地方。
            dir = match dir.open_dir_nofollow(segment) {
                Ok(next) => next,
                Err(_) => return Err(explain(&dir, segment, path)),
            };
        }
        Ok(dir)
    }
}

/// 字串規則(第一關):回傳一段一段的名稱,`.` 去掉。不碰檔案系統。
fn segments(path: &str) -> Result<Vec<&OsStr>, ToolError> {
    let invalid = |reason: &str| ToolError::InvalidArgs(format!("path {path:?} {reason}"));
    if path.len() > MAX_PATH_BYTES {
        return Err(ToolError::InvalidArgs(format!(
            "path is longer than {MAX_PATH_BYTES} bytes"
        )));
    }
    let mut segments = Vec::new();
    // `components()` 把路徑拆成一段一段,`..`、開頭的 `/` 各自是一種 Component。
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) if is_hidden(name) => {
                return Err(invalid(
                    "is hidden (names starting with `.` are not allowed)",
                ));
            }
            Component::Normal(name) => segments.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(invalid("must not contain `..`")),
            // `/etc/passwd`、Windows 的 `C:\`:絕對路徑。
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("must be relative to the allowed directory"));
            }
        }
    }
    Ok(segments)
}

/// `.env`、`.git`:最常放秘密的地方(design §10-5)。
pub(crate) fn is_hidden(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// 要寫入的目標現在的狀態:不存在(`None`)、一般檔案(它的 metadata),其他都是錯誤。
fn target(dir: &Dir, name: &OsStr, path: &str) -> Result<Option<cap_std::fs::Metadata>, ToolError> {
    match dir.symlink_metadata(name) {
        Ok(meta) if meta.is_symlink() => Err(symlink(path)),
        Ok(meta) if !meta.is_file() => {
            Err(ToolError::Failed(format!("{path:?} is not a regular file")))
        }
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ToolError::Failed(format!("{path:?} cannot be opened"))),
    }
}

/// 寫入暫存檔、設定權限、確定寫到磁碟上。
fn fill(
    mut file: File,
    bytes: &[u8],
    permissions: Option<cap_std::fs::Permissions>,
) -> io::Result<()> {
    file.write_all(bytes)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    file.sync_all()
}

/// 某一段開不起來:說明原因。安全性不靠這裡(靠 nofollow),這裡只是讓錯誤訊息有用。
fn explain(dir: &Dir, segment: &OsStr, path: &str) -> ToolError {
    match dir.symlink_metadata(segment) {
        Ok(meta) if meta.is_symlink() => symlink(path),
        Ok(meta) if !meta.is_dir() => ToolError::Failed(format!("{path:?} is not a directory")),
        Ok(_) => ToolError::Failed(format!("{path:?} cannot be opened")),
        Err(_) => not_found(path),
    }
}

fn symlink(path: &str) -> ToolError {
    ToolError::InvalidArgs(format!(
        "path {path:?} is or goes through a symbolic link (symbolic links are not followed)"
    ))
}

fn not_found(path: &str) -> ToolError {
    ToolError::Failed(format!("no such file or directory: {path:?}"))
}
