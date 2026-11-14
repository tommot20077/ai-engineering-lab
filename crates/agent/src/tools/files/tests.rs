use std::fs;

use jiff::Timestamp;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::tool::{Clock, ErasedTool};

/// 測試用的目錄:
///
/// ```text
/// <tmp>/
///   outside/secret.txt          根目錄外面
///   root/                       允許的目錄
///     notes.txt                 三行
///     empty.txt
///     sub/inner.md
///     .env  .git/config  sub/.hidden
///     big.txt (超過 256 KiB)   binary.bin (不是 UTF-8)
///     link_in  -> notes.txt     (unix)指向裡面:可以
///     link_out -> ../outside/secret.txt   指向外面:擋
///     link_dot -> .env          指向隱藏檔:擋
///     link_dir -> sub           指向目錄的 symlink
/// ```
struct Fixture {
    // 欄位沒有被讀,但要留著:`TempDir` 被 drop 時會刪掉整個目錄。
    _tmp: TempDir,
    root: Root,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    fs::create_dir(base.join("outside")).unwrap();
    fs::write(base.join("outside/secret.txt"), "TOP SECRET").unwrap();
    let root = base.join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join("notes.txt"), "first\nsecond\nthird\n").unwrap();
    fs::write(root.join("empty.txt"), "").unwrap();
    fs::write(root.join("sub/inner.md"), "# inner").unwrap();
    fs::write(root.join(".env"), "API_KEY=secret").unwrap();
    fs::write(root.join(".git/config"), "[core]").unwrap();
    fs::write(root.join("sub/.hidden"), "hidden").unwrap();
    fs::write(root.join("big.txt"), "x".repeat(256 * 1024 + 1)).unwrap();
    fs::write(root.join("binary.bin"), [0xff, 0xfe, 0x00]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("notes.txt", root.join("link_in")).unwrap();
        symlink("../outside/secret.txt", root.join("link_out")).unwrap();
        symlink(".env", root.join("link_dot")).unwrap();
        symlink("sub", root.join("link_dir")).unwrap();
    }
    Fixture {
        root: Root::new(&root).unwrap(),
        _tmp: tmp,
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        run_id: Uuid::nil(),
        call_id: "toolu_1".into(),
        clock: Clock::Fixed(Timestamp::UNIX_EPOCH),
    }
}

async fn read(f: &Fixture, args: serde_json::Value) -> Result<String, ToolError> {
    ReadFile::new(f.root.clone())
        .call_json(&args.to_string(), &ctx())
        .await
}

async fn list(f: &Fixture, args: serde_json::Value) -> Result<String, ToolError> {
    ListDir::new(f.root.clone())
        .call_json(&args.to_string(), &ctx())
        .await
}

fn invalid(message: &str) -> Result<String, ToolError> {
    Err(ToolError::InvalidArgs(message.into()))
}

fn failed(message: &str) -> Result<String, ToolError> {
    Err(ToolError::Failed(message.into()))
}

// ---- read_file ----

#[tokio::test]
async fn reads_a_file_with_line_numbers() {
    let f = fixture();
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt"})).await,
        Ok("1\tfirst\n2\tsecond\n3\tthird".to_string())
    );
    // `./`、子目錄都可以。
    assert_eq!(
        read(&f, serde_json::json!({"path": "./sub/inner.md"})).await,
        Ok("1\t# inner".to_string())
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "empty.txt"})).await,
        Ok("(empty file)".to_string())
    );
}

#[tokio::test]
async fn offset_and_limit_pick_a_slice() {
    let f = fixture();
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt", "offset": 2})).await,
        Ok("2\tsecond\n3\tthird\n(lines 2-3 of 3)".to_string())
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt", "limit": 1})).await,
        Ok("1\tfirst\n(lines 1-1 of 3)".to_string())
    );
    assert_eq!(
        read(
            &f,
            serde_json::json!({"path": "notes.txt", "offset": 2, "limit": 1})
        )
        .await,
        Ok("2\tsecond\n(lines 2-2 of 3)".to_string())
    );
    // limit 超過檔尾:讀到最後一行為止,而且是整個檔案,不加註明。
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt", "limit": 99})).await,
        Ok("1\tfirst\n2\tsecond\n3\tthird".to_string())
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt", "offset": 4})).await,
        Ok("(the file has only 3 lines)".to_string())
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt", "offset": 0})).await,
        invalid("offset starts at 1")
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "notes.txt", "limit": 0})).await,
        invalid("limit must be at least 1")
    );
}

#[tokio::test]
async fn paths_that_leave_the_directory_are_rejected() {
    let f = fixture();
    for (path, expected) in [
        (
            "/etc/passwd",
            r#"path "/etc/passwd" must be relative to the allowed directory"#,
        ),
        (
            "../outside/secret.txt",
            r#"path "../outside/secret.txt" must not contain `..`"#,
        ),
        (
            "sub/../../outside/secret.txt",
            r#"path "sub/../../outside/secret.txt" must not contain `..`"#,
        ),
        // 留在裡面的 `..` 也不收:規則越簡單越不會出錯。
        (
            "sub/../notes.txt",
            r#"path "sub/../notes.txt" must not contain `..`"#,
        ),
    ] {
        assert_eq!(
            read(&f, serde_json::json!({ "path": path })).await,
            invalid(expected),
            "{path}"
        );
    }
}

#[tokio::test]
async fn hidden_files_are_rejected() {
    let f = fixture();
    for path in [".env", ".git/config", "sub/.hidden", "./.env"] {
        assert_eq!(
            read(&f, serde_json::json!({ "path": path })).await,
            invalid(&format!(
                "path {path:?} is hidden (names starting with `.` are not readable)"
            )),
            "{path}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_followed_only_when_they_stay_inside() {
    let f = fixture();
    assert_eq!(
        read(&f, serde_json::json!({"path": "link_in"})).await,
        Ok("1\tfirst\n2\tsecond\n3\tthird".to_string())
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "link_out"})).await,
        invalid(r#"path "link_out" points outside the allowed directory"#)
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "link_dot"})).await,
        invalid(r#"path "link_dot" points to a hidden file"#)
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "link_dir/inner.md"})).await,
        Ok("1\t# inner".to_string())
    );
}

#[tokio::test]
async fn only_small_utf8_regular_files_are_read() {
    let f = fixture();
    assert_eq!(
        read(&f, serde_json::json!({"path": "missing.txt"})).await,
        failed(r#"no such file or directory: "missing.txt""#)
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "sub"})).await,
        failed(r#""sub" is not a regular file"#)
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "big.txt"})).await,
        failed(r#""big.txt" is larger than 256 KiB"#)
    );
    assert_eq!(
        read(&f, serde_json::json!({"path": "binary.bin"})).await,
        failed(r#""binary.bin" is not a UTF-8 text file"#)
    );
}

#[tokio::test]
async fn a_file_of_exactly_the_limit_is_read() {
    let f = fixture();
    let exact = f.root.canonical.join("exact.txt");
    fs::write(&exact, "y".repeat(256 * 1024)).unwrap();
    let result = read(&f, serde_json::json!({"path": "exact.txt"})).await;
    assert!(
        result.as_ref().is_ok_and(|text| text.starts_with("1\tyyy")),
        "{result:?}"
    );
}

#[tokio::test]
async fn very_long_paths_are_rejected_before_touching_the_disk() {
    let f = fixture();
    let long = "a/".repeat(513);
    assert_eq!(
        read(&f, serde_json::json!({ "path": long })).await,
        invalid("path is longer than 1024 bytes")
    );
}

// ---- list_dir ----

#[tokio::test]
async fn lists_a_directory_sorted_without_hidden_entries() {
    let f = fixture();
    let expected = if cfg!(unix) {
        "big.txt\nbinary.bin\nempty.txt\nlink_dir\nlink_dot\nlink_in\nlink_out\nnotes.txt\nsub/"
    } else {
        "big.txt\nbinary.bin\nempty.txt\nnotes.txt\nsub/"
    };
    assert_eq!(
        list(&f, serde_json::json!({})).await,
        Ok(expected.to_string())
    );
    assert_eq!(
        list(&f, serde_json::json!({"path": "."})).await,
        Ok(expected.to_string())
    );
    assert_eq!(
        list(&f, serde_json::json!({"path": "sub"})).await,
        Ok("inner.md".to_string())
    );
}

#[tokio::test]
async fn list_dir_follows_the_same_path_rules() {
    let f = fixture();
    assert_eq!(
        list(&f, serde_json::json!({"path": ".."})).await,
        invalid(r#"path ".." must not contain `..`"#)
    );
    assert_eq!(
        list(&f, serde_json::json!({"path": ".git"})).await,
        invalid(r#"path ".git" is hidden (names starting with `.` are not readable)"#)
    );
    assert_eq!(
        list(&f, serde_json::json!({"path": "notes.txt"})).await,
        failed(r#""notes.txt" is not a directory"#)
    );
}

#[tokio::test]
async fn an_empty_directory_says_so_and_long_listings_are_capped() {
    let f = fixture();
    fs::create_dir(f.root.canonical.join("nothing")).unwrap();
    assert_eq!(
        list(&f, serde_json::json!({"path": "nothing"})).await,
        Ok("(empty directory)".to_string())
    );
    let many = f.root.canonical.join("many");
    fs::create_dir(&many).unwrap();
    for i in 0..MAX_ENTRIES + 2 {
        fs::write(many.join(format!("f{i:04}")), "").unwrap();
    }
    let listing = list(&f, serde_json::json!({"path": "many"})).await.unwrap();
    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(lines.len(), MAX_ENTRIES + 1);
    assert_eq!(lines[0], "f0000");
    assert_eq!(lines[MAX_ENTRIES - 1], "f0499");
    assert_eq!(lines[MAX_ENTRIES], "(and 2 more)");
}

// ---- 根目錄 ----

#[test]
fn the_root_must_be_an_existing_directory() {
    let f = fixture();
    assert!(Root::new(f.root.canonical.join("missing")).is_err());
    let error = Root::new(f.root.canonical.join("notes.txt")).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
}

#[test]
fn the_definitions_sent_to_the_model_are_locked() {
    let f = fixture();
    insta::assert_json_snapshot!("read_file", ReadFile::new(f.root.clone()).def());
    insta::assert_json_snapshot!("list_dir", ListDir::new(f.root.clone()).def());
}
