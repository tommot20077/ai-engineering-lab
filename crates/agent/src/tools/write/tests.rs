use std::fs;

use jiff::Timestamp;
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::tool::{Clock, ErasedTool};

/// 測試用的目錄:
///
/// ```text
/// <tmp>/
///   outside/secret.txt         根目錄外面
///   root/                      允許的目錄
///     notes.txt                "first\nsecond\nthird\n"
///     sub/
///     .env
///     link_file -> notes.txt   (unix)
///     link_out  -> ../outside/secret.txt
///     link_dir  -> sub
/// ```
struct Fixture {
    tmp: TempDir,
    root: Root,
}

impl Fixture {
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.tmp.path().join("root").join(name)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap()
    }

    /// 根目錄裡所有的名稱(含隱藏檔):確認沒有留下暫存檔。
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.tmp.path().join("root"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn secret(&self) -> String {
        fs::read_to_string(self.tmp.path().join("outside/secret.txt")).unwrap()
    }
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    fs::create_dir(base.join("outside")).unwrap();
    fs::write(base.join("outside/secret.txt"), "TOP SECRET").unwrap();
    let root = base.join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("notes.txt"), "first\nsecond\nthird\n").unwrap();
    fs::write(root.join(".env"), "API_KEY=secret").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("notes.txt", root.join("link_file")).unwrap();
        symlink("../outside/secret.txt", root.join("link_out")).unwrap();
        symlink("sub", root.join("link_dir")).unwrap();
    }
    Fixture {
        root: Root::new(&root).unwrap(),
        tmp,
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        run_id: Uuid::nil(),
        call_id: "toolu_1".into(),
        clock: Clock::Fixed(Timestamp::UNIX_EPOCH),
    }
}

async fn write(f: &Fixture, args: serde_json::Value) -> Result<String, ToolError> {
    WriteFile::new(f.root.clone())
        .call_json(&args.to_string(), &ctx())
        .await
}

async fn edit(f: &Fixture, args: serde_json::Value) -> Result<String, ToolError> {
    EditFile::new(f.root.clone())
        .call_json(&args.to_string(), &ctx())
        .await
}

async fn describe_write(f: &Fixture, args: serde_json::Value) -> String {
    WriteFile::new(f.root.clone())
        .describe_json(&args.to_string(), &ctx())
        .await
}

async fn describe_edit(f: &Fixture, args: serde_json::Value) -> String {
    EditFile::new(f.root.clone())
        .describe_json(&args.to_string(), &ctx())
        .await
}

fn invalid(message: &str) -> Result<String, ToolError> {
    Err(ToolError::InvalidArgs(message.into()))
}

fn failed(message: &str) -> Result<String, ToolError> {
    Err(ToolError::Failed(message.into()))
}

// ---- write_file ----

#[tokio::test]
async fn write_file_creates_a_new_file() {
    let f = fixture();
    assert_eq!(
        write(&f, json!({"path": "sub/todo.md", "content": "- 買牛奶\n"})).await,
        Ok("created sub/todo.md (12 bytes)".to_string())
    );
    assert_eq!(f.read("sub/todo.md"), "- 買牛奶\n");
}

#[tokio::test]
async fn write_file_overwrites_and_leaves_no_temporary_file() {
    let f = fixture();
    let before = f.names();
    assert_eq!(
        write(&f, json!({"path": "notes.txt", "content": "new"})).await,
        Ok("overwrote notes.txt (3 bytes)".to_string())
    );
    assert_eq!(f.read("notes.txt"), "new");
    assert_eq!(f.names(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn overwriting_keeps_the_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    fs::set_permissions(f.path("notes.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    write(&f, json!({"path": "notes.txt", "content": "new"}))
        .await
        .unwrap();
    let mode = fs::metadata(f.path("notes.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o640);
}

#[tokio::test]
async fn write_file_follows_the_path_rules() {
    let f = fixture();
    for (path, expected) in [
        (
            ".env",
            invalid(r#"path ".env" is hidden (names starting with `.` are not allowed)"#),
        ),
        (
            "sub/../x",
            invalid(r#"path "sub/../x" must not contain `..`"#),
        ),
        (
            "/tmp/x",
            invalid(r#"path "/tmp/x" must be relative to the allowed directory"#),
        ),
        (".", invalid(r#"path "." must name a file"#)),
        ("sub", failed(r#""sub" is not a regular file"#)),
        (
            "missing/x.txt",
            failed(r#"no such file or directory: "missing/x.txt""#),
        ),
        ("notes.txt/x", failed(r#""notes.txt/x" is not a directory"#)),
    ] {
        assert_eq!(
            write(&f, json!({"path": path, "content": "x"})).await,
            expected,
            "{path}"
        );
    }
    assert_eq!(f.read(".env"), "API_KEY=secret");
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_never_writes_through_a_symlink() {
    let f = fixture();
    for path in ["link_file", "link_out", "link_dir/x.txt"] {
        assert_eq!(
            write(&f, json!({"path": path, "content": "pwned"})).await,
            invalid(&format!(
                "path {path:?} is or goes through a symbolic link (symbolic links are not followed)"
            )),
            "{path}"
        );
    }
    assert_eq!(f.secret(), "TOP SECRET");
    assert_eq!(f.read("notes.txt"), "first\nsecond\nthird\n");
    assert!(!f.path("sub/x.txt").exists());
}

#[tokio::test]
async fn write_file_refuses_content_over_the_limit() {
    let f = fixture();
    let big = "x".repeat(256 * 1024 + 1);
    assert_eq!(
        write(&f, json!({"path": "big.txt", "content": big})).await,
        failed(r#""big.txt" would be larger than 256 KiB"#)
    );
    assert!(!f.path("big.txt").exists());
    let exact = "x".repeat(256 * 1024);
    assert!(
        write(&f, json!({"path": "big.txt", "content": exact}))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn write_file_describes_what_it_will_do() {
    let f = fixture();
    assert_eq!(
        describe_write(&f, json!({"path": "new.md", "content": "a\nb"})).await,
        "create new.md (3 bytes)\n  a\n  b"
    );
    assert_eq!(
        describe_write(&f, json!({"path": "notes.txt", "content": "x"})).await,
        "overwrite notes.txt (19 bytes -> 1 bytes)\n  x"
    );
    assert_eq!(
        describe_write(&f, json!({"path": "sub", "content": "x"})).await,
        r#""sub" is not a regular file (nothing will be written)"#
    );
}

// ---- edit_file ----

#[tokio::test]
async fn edit_file_replaces_text_that_appears_exactly_once() {
    let f = fixture();
    assert_eq!(
        edit(
            &f,
            json!({"path": "notes.txt", "old": "second\n", "new": "2nd\n2.5th\n"})
        )
        .await,
        Ok("edited notes.txt at line 2".to_string())
    );
    assert_eq!(f.read("notes.txt"), "first\n2nd\n2.5th\nthird\n");
}

#[tokio::test]
async fn edit_file_changes_nothing_unless_old_appears_exactly_once() {
    let f = fixture();
    fs::write(f.path("notes.txt"), "aaa\nbab\n").unwrap();
    assert_eq!(
        edit(&f, json!({"path": "notes.txt", "old": "zzz", "new": "y"})).await,
        failed(r#"`old` was not found in "notes.txt"; nothing was changed"#)
    );
    let twice = failed(
        r#"`old` appears more than once in "notes.txt"; include more surrounding text so it appears exactly once. Nothing was changed"#,
    );
    // "b" 出現兩次;"aa" 在 "aaa" 裡重疊出現兩次。
    for old in ["b", "aa"] {
        assert_eq!(
            edit(&f, json!({"path": "notes.txt", "old": old, "new": "y"})).await,
            twice,
            "{old}"
        );
    }
    assert_eq!(
        edit(&f, json!({"path": "notes.txt", "old": "", "new": "y"})).await,
        invalid("`old` must not be empty")
    );
    assert_eq!(f.read("notes.txt"), "aaa\nbab\n");
    // 多包一點前後文就只出現一次。
    assert_eq!(
        edit(&f, json!({"path": "notes.txt", "old": "bab", "new": "b"})).await,
        Ok("edited notes.txt at line 2".to_string())
    );
    assert_eq!(f.read("notes.txt"), "aaa\nb\n");
}

#[tokio::test]
async fn edit_file_can_delete_and_handles_multibyte_text() {
    let f = fixture();
    fs::write(f.path("notes.txt"), "早安\n午安\n晚安\n").unwrap();
    assert_eq!(
        edit(&f, json!({"path": "notes.txt", "old": "午安\n", "new": ""})).await,
        Ok("edited notes.txt at line 2".to_string())
    );
    assert_eq!(f.read("notes.txt"), "早安\n晚安\n");
}

#[cfg(unix)]
#[tokio::test]
async fn edit_file_never_edits_through_a_symlink() {
    let f = fixture();
    assert_eq!(
        edit(&f, json!({"path": "link_out", "old": "TOP", "new": "NOT"})).await,
        invalid(
            r#"path "link_out" is or goes through a symbolic link (symbolic links are not followed)"#
        )
    );
    assert_eq!(f.secret(), "TOP SECRET");
}

#[tokio::test]
async fn edit_file_describes_the_change() {
    let f = fixture();
    assert_eq!(
        describe_edit(
            &f,
            json!({"path": "notes.txt", "old": "second\nthird", "new": "2nd"})
        )
        .await,
        "edit notes.txt at line 2\n- second\n- third\n+ 2nd"
    );
    assert_eq!(
        describe_edit(&f, json!({"path": "notes.txt", "old": "zzz", "new": "y"})).await,
        r#"`old` was not found in "notes.txt"; nothing was changed (nothing will change)"#
    );
}

#[tokio::test]
async fn edit_file_rechecks_the_file_when_it_runs() {
    // 核准的時候 `old` 還在,執行之前被別人改掉了:不改。
    let f = fixture();
    let args = json!({"path": "notes.txt", "old": "second", "new": "2nd"});
    assert!(
        describe_edit(&f, args.clone())
            .await
            .starts_with("edit notes.txt")
    );
    fs::write(f.path("notes.txt"), "changed by someone else\n").unwrap();
    assert_eq!(
        edit(&f, args).await,
        failed(r#"`old` was not found in "notes.txt"; nothing was changed"#)
    );
    assert_eq!(f.read("notes.txt"), "changed by someone else\n");
}

#[tokio::test]
async fn edit_file_refuses_a_result_over_the_limit() {
    let f = fixture();
    let big = "x".repeat(256 * 1024);
    assert_eq!(
        edit(&f, json!({"path": "notes.txt", "old": "first", "new": big})).await,
        failed(r#""notes.txt" would be larger than 256 KiB; nothing was changed"#)
    );
    assert_eq!(f.read("notes.txt"), "first\nsecond\nthird\n");
}

// ---- 共通 ----

#[test]
fn both_tools_need_approval_and_the_read_tools_do_not() {
    let f = fixture();
    assert!(ErasedTool::needs_approval(&WriteFile::new(f.root.clone())));
    assert!(ErasedTool::needs_approval(&EditFile::new(f.root.clone())));
    assert!(!ErasedTool::needs_approval(&crate::tools::ReadFile::new(
        f.root.clone()
    )));
    assert!(!ErasedTool::needs_approval(&crate::tools::ListDir::new(
        f.root.clone()
    )));
}

#[test]
fn previews_are_capped_by_lines_and_bytes() {
    let many: String = (1..=30).map(|n| format!("line {n}\n")).collect();
    let shown = preview(&many, "> ");
    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(lines.len(), PREVIEW_LINES + 1);
    assert_eq!(lines[0], "> line 1");
    assert_eq!(lines[PREVIEW_LINES - 1], "> line 20");
    assert_eq!(lines[PREVIEW_LINES], "> ... (30 lines in total)");

    let wide = "é".repeat(1500); // 3000 bytes,一行
    let shown = preview(&wide, "");
    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(lines[0].len(), PREVIEW_BYTES);
    assert_eq!(lines[1], "... (1 lines in total)");

    assert_eq!(preview("", "  "), "  (empty)");
    assert_eq!(preview("a\nb", "  "), "  a\n  b");
}

#[test]
fn the_definitions_sent_to_the_model_are_locked() {
    let f = fixture();
    insta::assert_json_snapshot!("write_file", WriteFile::new(f.root.clone()).def());
    insta::assert_json_snapshot!("edit_file", EditFile::new(f.root.clone()).def());
}
