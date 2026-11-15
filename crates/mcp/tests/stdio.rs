//! stdio 傳輸對測試 server(`mcp-test-server --stdio`)的整合測試(design-part2 §5、§7)。
//!
//! 每個測試都啟動一個真的子行程:id 對應、並行、server 反問、環境變數、上限、server 死掉、關閉,全部走真的 pipe。

// 整個檔案都是測試:helper 函式不在 `#[test]` 裡,clippy.toml 的「測試裡放寬」管不到它們(同 agent 的 E2E)。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use mcp::jsonrpc::{METHOD_NOT_FOUND, RpcError};
use mcp::transport::{StdioConfig, StdioLimits, StdioTransport, Transport};
use mcp::{McpError, transport::stdio};
use serde_json::{Value, json};

const SERVER: &str = env!("CARGO_BIN_EXE_mcp-test-server");

/// 錯的實作可能讓請求永遠等不到回應:包一層,讓測試「失敗」而不是卡到 CI 逾時。
async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("timed out: the transport is stuck")
}

fn config() -> StdioConfig {
    StdioConfig {
        command: SERVER.into(),
        args: vec!["--stdio".into()],
        env: BTreeMap::new(),
    }
}

fn start() -> StdioTransport {
    StdioTransport::spawn(&config(), StdioLimits::default()).unwrap()
}

async fn call(t: &StdioTransport, method: &str, params: Value) -> Result<Value, McpError> {
    within(t.request(method, Some(params))).await
}

/// 行程還在不在。`kill -0` 只檢查、不送訊號;已經結束但還沒被收掉的(zombie)也算不在。
fn alive(pid: u64) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

// ---- 請求與回應 ----

#[tokio::test]
async fn a_request_gets_its_response() {
    let t = start();
    assert_eq!(
        call(&t, "test/echo", json!({"a": [1, "two"]})).await,
        Ok(json!({"a": [1, "two"]}))
    );
    // 沒有 params 的請求(`ping`)。
    assert_eq!(within(t.request("ping", None)).await, Ok(json!({})));
}

#[tokio::test]
async fn concurrent_requests_are_matched_by_id() {
    // 先送的比較慢:回應的順序跟請求相反。照順序交回去的實作會把 "fast" 交給第一個請求。
    let t = start();
    let (slow, fast) = within(async {
        tokio::join!(
            t.request("test/sleep", Some(json!({"ms": 300, "value": "slow"}))),
            t.request("test/sleep", Some(json!({"ms": 10, "value": "fast"}))),
        )
    })
    .await;
    assert_eq!(slow, Ok(json!("slow")));
    assert_eq!(fast, Ok(json!("fast")));
    assert_eq!(t.in_flight(), 0);
}

#[tokio::test]
async fn an_error_response_is_an_rpc_error_and_the_connection_stays_open() {
    let t = start();
    assert_eq!(
        call(&t, "no/such/method", json!({})).await,
        Err(McpError::Rpc(RpcError {
            code: METHOD_NOT_FOUND,
            message: "method not found: no/such/method".into(),
            data: None,
        }))
    );
    assert_eq!(call(&t, "test/echo", json!(1)).await, Ok(json!(1)));
}

#[tokio::test]
async fn notifications_from_the_server_are_skipped() {
    let t = start();
    assert_eq!(
        call(&t, "test/notify", json!({})).await,
        Ok(json!("after the notification"))
    );
}

#[tokio::test]
async fn requests_from_the_server_are_refused_with_method_not_found() {
    // server 反問 `sampling/createMessage`;它把收到的回應原樣交回來。
    let t = start();
    let answer = call(&t, "test/ask", json!({})).await.unwrap();
    assert_eq!(answer["jsonrpc"], "2.0");
    assert_eq!(answer["id"], "s-1");
    assert_eq!(answer["error"]["code"], METHOD_NOT_FOUND);
    assert!(answer.get("result").is_none(), "{answer}");
}

#[tokio::test]
async fn blank_lines_are_skipped() {
    let t = start();
    assert_eq!(
        call(&t, "test/blank-line", json!({})).await,
        Ok(json!("after the blank lines"))
    );
}

#[tokio::test]
async fn a_response_with_an_unknown_id_is_ignored() {
    let t = start();
    assert_eq!(
        call(&t, "test/unknown-id", json!({})).await,
        Ok(json!("real"))
    );
    assert_eq!(call(&t, "test/echo", json!(2)).await, Ok(json!(2)));
}

#[tokio::test]
async fn a_request_that_is_given_up_on_is_forgotten() {
    // 呼叫端不等了(逾時、被取消):future 被 drop,等待名單裡的那一筆也要拿掉,不然每次逾時都漏一點記憶體。
    let t = start();
    let mut request = t.request("test/sleep", Some(json!({"ms": 300, "value": "late"})));
    // `&mut request`:等一下但不拿走;逾時之後 future 還在,還在等待名單裡。
    let given_up = tokio::time::timeout(Duration::from_millis(50), &mut request).await;
    assert!(given_up.is_err());
    assert_eq!(t.in_flight(), 1);
    drop(request);
    assert_eq!(t.in_flight(), 0);
    // 晚到的回應沒人等,略過;連線照常。
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(call(&t, "test/echo", json!(3)).await, Ok(json!(3)));
}

#[tokio::test]
async fn a_request_that_is_given_up_on_is_cancelled() {
    // 規格(stdio「Cancellation」):放棄的請求要送 `notifications/cancelled`,server 才能停下來。
    // 這是這條連線的第一個請求,id 是 1。取消通知由背景 task 送出,稍等一下才到。
    let t = start();
    let request = t.request("test/sleep", Some(json!({"ms": 2000, "value": "late"})));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), request)
            .await
            .is_err()
    );
    let mut cancelled = json!([]);
    for _ in 0..50 {
        cancelled = call(&t, "test/cancelled", json!({})).await.unwrap();
        if cancelled != json!([]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(cancelled, json!([1]));
}

#[tokio::test]
async fn a_request_that_got_its_answer_is_not_cancelled() {
    let t = start();
    assert_eq!(call(&t, "test/echo", json!(1)).await, Ok(json!(1)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(call(&t, "test/cancelled", json!({})).await, Ok(json!([])));
}

#[tokio::test]
async fn notify_sends_without_waiting() {
    // 測試 server 不回應通知;送出後連線照常(通知沒有 id,不會被當成請求等下去)。
    let t = start();
    assert_eq!(
        within(t.notify("notifications/initialized", None)).await,
        Ok(())
    );
    assert_eq!(t.in_flight(), 0);
    assert_eq!(call(&t, "test/echo", json!(4)).await, Ok(json!(4)));
}

// ---- 環境變數(D-050)----

#[tokio::test]
async fn the_child_gets_only_the_configured_environment() {
    // 測試行程本身有一堆環境變數(CARGO_*、PATH……,CI 上可能有 key);子行程只能看到設定檔寫的。
    let mut config = config();
    config.env.insert("MCP_TEST_FOO".into(), "bar".into());
    let t = StdioTransport::spawn(&config, StdioLimits::default()).unwrap();
    let env = call(&t, "test/env", json!({})).await.unwrap();
    assert_eq!(env["names"], json!(["MCP_TEST_FOO"]));
    assert_eq!(env["values"], json!({"MCP_TEST_FOO": "bar"}));
}

// ---- 上限與違反協定:關掉連線(fail-closed)----

#[tokio::test]
async fn a_message_over_the_limit_closes_the_connection() {
    let limits = StdioLimits {
        max_message_bytes: 1000,
        ..StdioLimits::default()
    };
    let t = StdioTransport::spawn(&config(), limits).unwrap();
    // 900 個字加上外面的 JSON 還在上限內。
    let small = call(&t, "test/big", json!({"bytes": 900})).await.unwrap();
    assert_eq!(small.as_str().map(str::len), Some(900));

    let closed = Err(McpError::TooLarge { limit: 1000 });
    assert_eq!(call(&t, "test/big", json!({"bytes": 1000})).await, closed);
    // 之後的請求立刻得到同一個原因,不會再送出去等。
    assert_eq!(call(&t, "test/echo", json!(5)).await, closed);
}

#[tokio::test]
async fn the_default_limits() {
    let limits = StdioLimits::default();
    assert_eq!(limits.max_message_bytes, 4 * 1024 * 1024);
    assert_eq!(limits.close_grace, Duration::from_secs(2));
}

#[tokio::test]
async fn a_line_that_is_not_json_rpc_closes_the_connection() {
    let t = start();
    let err = call(&t, "test/garbage", json!({})).await.unwrap_err();
    let McpError::Protocol(reason) = &err else {
        panic!("{err:?}")
    };
    assert!(reason.starts_with("not JSON: "), "{reason}");
    assert_eq!(call(&t, "test/echo", json!(6)).await, Err(err.clone()));
    // server 還活著、pipe 也還通,但連線已經不能信任:通知也不送。
    assert_eq!(
        within(t.notify("notifications/initialized", None)).await,
        Err(err)
    );
}

#[tokio::test]
async fn a_line_that_is_not_utf8_closes_the_connection() {
    let t = start();
    assert_eq!(
        call(&t, "test/not-utf8", json!({})).await,
        Err(McpError::Protocol("not UTF-8".into()))
    );
}

// ---- server 死掉 ----

#[tokio::test]
async fn the_server_exiting_fails_the_pending_request_with_its_stderr() {
    let t = start();
    assert_eq!(
        call(
            &t,
            "test/exit",
            json!({"stderr": "boom: no config\n", "code": 3})
        )
        .await,
        Err(McpError::Closed {
            stderr: "boom: no config".into()
        })
    );
    // 後來的請求也是同一個原因。
    assert_eq!(
        call(&t, "test/echo", json!(7)).await,
        Err(McpError::Closed {
            stderr: "boom: no config".into()
        })
    );
}

#[tokio::test]
async fn stderr_written_after_stdout_closes_still_makes_it_into_the_error() {
    // stdout 先關、0.3 秒後才寫 stderr:看到 stdout 結束時,stderr 還沒有東西。要等 stderr 也結束才收尾,
    // 錯誤訊息裡才有 server 最後說的話。用 sh 才做得到「只關 stdout」(測試 server 禁止 unsafe,關不了 fd 1)。
    let config = StdioConfig {
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "exec 1>&-; sleep 0.3; echo 'late words' >&2; exit 1".into(),
        ],
        env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
    };
    let t = StdioTransport::spawn(&config, StdioLimits::default()).unwrap();
    assert_eq!(
        call(&t, "test/echo", json!(8)).await,
        Err(McpError::Closed {
            stderr: "late words".into()
        })
    );
}

#[tokio::test]
async fn half_a_line_then_exit_is_a_closed_connection() {
    let t = start();
    assert_eq!(
        call(&t, "test/half-line", json!({})).await,
        Err(McpError::Closed {
            stderr: String::new()
        })
    );
}

#[tokio::test]
async fn stderr_is_drained_and_only_its_tail_is_kept() {
    // 200 KB 遠超過 pipe 的容量:沒有持續讀 stderr 的話,server 卡住,這個請求永遠等不到(`within` 會讓它失敗)。
    let t = start();
    assert_eq!(
        call(&t, "test/stderr", json!({"bytes": 200_000})).await,
        Ok(json!("done"))
    );
    let err = call(&t, "test/exit", json!({"stderr": "last words", "code": 1}))
        .await
        .unwrap_err();
    let McpError::Closed { stderr } = err else {
        panic!("{err:?}")
    };
    assert_eq!(stderr.len(), stdio::STDERR_TAIL_BYTES);
    assert!(stderr.ends_with("eeelast words"), "{stderr}");
}

#[tokio::test]
async fn a_command_that_cannot_start_is_a_spawn_error() {
    let mut config = config();
    config.command = "/nonexistent/mcp-server".into();
    let err = StdioTransport::spawn(&config, StdioLimits::default())
        .err()
        .unwrap();
    let McpError::Spawn { command, reason } = err else {
        panic!("{err:?}")
    };
    assert_eq!(command, "/nonexistent/mcp-server");
    assert!(!reason.is_empty());
}

// ---- 關閉 ----

#[tokio::test]
async fn shutdown_lets_a_well_behaved_server_exit_on_its_own() {
    // 關掉 stdin,server 自己結束(exit code 0);不必等到寬限時間、也不必 kill。
    let limits = StdioLimits {
        close_grace: Duration::from_secs(5),
        ..StdioLimits::default()
    };
    let t = StdioTransport::spawn(&config(), limits).unwrap();
    let pid = call(&t, "test/pid", json!({}))
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    let started = Instant::now();
    let status = within(t.shutdown()).await.unwrap();
    assert!(status.success(), "{status:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!alive(pid));
}

#[tokio::test]
async fn shutdown_kills_a_server_that_ignores_stdin_closing() {
    let limits = StdioLimits {
        close_grace: Duration::from_millis(200),
        ..StdioLimits::default()
    };
    let t = StdioTransport::spawn(&config(), limits).unwrap();
    call(&t, "test/linger", json!({})).await.unwrap();
    let pid = call(&t, "test/pid", json!({}))
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    let status = within(t.shutdown()).await.unwrap();
    // 被 kill 的行程沒有 exit code(是被訊號結束的)。
    assert_eq!(status.code(), None);
    assert!(!alive(pid));
}

#[tokio::test]
async fn dropping_the_transport_kills_the_server() {
    // 沒有呼叫 shutdown 就丟掉(agent 因為錯誤提早結束):`kill_on_drop` 保證 server 不會留下來。
    let t = start();
    let pid = call(&t, "test/pid", json!({}))
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    drop(t);
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(pid));
}

#[tokio::test]
async fn close_through_the_trait_object() {
    // `McpClient` 拿到的是 `Box<dyn Transport>`:經由 trait 關閉也要真的關掉。
    let t = start();
    let pid = call(&t, "test/pid", json!({}))
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    let boxed: Box<dyn Transport> = Box::new(t);
    within(boxed.close()).await;
    assert!(!alive(pid));
}
