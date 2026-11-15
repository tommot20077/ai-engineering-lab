use serde_json::json;

use super::*;

#[test]
fn only_https_or_local_http_urls_are_accepted() {
    for ok in [
        "https://example.com/mcp",
        "http://localhost:8080/mcp",
        "http://127.0.0.1:3000/mcp",
        "http://[::1]:3000/mcp",
    ] {
        assert!(check_url(ok).is_ok(), "{ok}");
    }
    let plain = Err(McpError::InvalidUrl(
        "must use https (plain http is only allowed for localhost)".into(),
    ));
    for bad in [
        "http://example.com/mcp",
        // 看起來像 localhost 的別的網域。
        "http://localhost.evil.com/mcp",
        "ftp://localhost/mcp",
        "file:///tmp/mcp",
    ] {
        assert_eq!(check_url(bad).map(|_| ()), plain.clone(), "{bad}");
    }
    assert!(matches!(
        check_url("not a url"),
        Err(McpError::InvalidUrl(_))
    ));
    assert_eq!(
        check_url("https://user:pass@example.com/mcp").map(|_| ()),
        Err(McpError::InvalidUrl(
            "credentials in the URL are not allowed; use a token".into()
        ))
    );
    assert!(check_url("https://user@example.com/mcp").is_err());
    // 只有密碼、沒有帳號。
    assert!(check_url("https://:pass@example.com/mcp").is_err());
}

#[test]
fn the_config_does_not_print_the_token() {
    let config = HttpConfig {
        url: "https://example.com/mcp".into(),
        token: Some(SecretString::from("tok-very-secret")),
    };
    let debug = format!("{config:?}");
    assert!(!debug.contains("tok-very-secret"), "{debug}");
    assert!(debug.contains("REDACTED"), "{debug}");
}

#[test]
fn the_default_limits() {
    let limits = HttpLimits::default();
    assert_eq!(limits.max_message_bytes, 4 * 1024 * 1024);
    assert_eq!(limits.max_stream_bytes, 16 * 1024 * 1024);
    assert_eq!(limits.connect_timeout, Duration::from_secs(10));
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| v.to_str().unwrap().to_owned())
}

#[test]
fn body_values_are_mirrored_into_headers() {
    let params = json!({
        "name": "get_weather",
        "arguments": {"location": "Seattle"},
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
    });
    let headers = metadata_headers(
        "tools/call",
        Some(&params),
        vec![("Region".into(), "us-west1".into())],
    )
    .unwrap();
    assert_eq!(
        header(&headers, "mcp-method").as_deref(),
        Some("tools/call")
    );
    assert_eq!(
        header(&headers, "mcp-protocol-version").as_deref(),
        Some("2026-07-28")
    );
    assert_eq!(header(&headers, "mcp-name").as_deref(), Some("get_weather"));
    assert_eq!(
        header(&headers, "mcp-param-region").as_deref(),
        Some("us-west1")
    );
    assert_eq!(headers.len(), 4);
}

#[test]
fn mcp_name_is_only_for_named_methods() {
    // tools/list 沒有 Mcp-Name;resources/read 用 uri;不安全的值要編碼。
    let params = json!({"name": "ignored", "uri": "file:///a b"});
    let headers = metadata_headers("tools/list", Some(&params), vec![]).unwrap();
    assert_eq!(header(&headers, "mcp-name"), None);
    let headers = metadata_headers("resources/read", Some(&params), vec![]).unwrap();
    assert_eq!(header(&headers, "mcp-name").as_deref(), Some("file:///a b"));
    let params = json!({"name": "天氣"});
    let headers = metadata_headers("prompts/get", Some(&params), vec![]).unwrap();
    assert_eq!(
        header(&headers, "mcp-name").as_deref(),
        Some("=?base64?5aSp5rCj?=")
    );
    // 沒有 params、沒有 _meta:只有 Mcp-Method。
    let headers = metadata_headers("ping", None, vec![]).unwrap();
    assert_eq!(headers.len(), 1);
}

#[test]
fn an_invalid_header_name_is_refused() {
    let err =
        metadata_headers("tools/call", None, vec![("bad name".into(), "v".into())]).unwrap_err();
    assert_eq!(
        err,
        McpError::Protocol("invalid header name `Mcp-Param-bad name`".into())
    );
}

#[test]
fn messages_on_a_response_are_sorted_out() {
    // 這個請求的回應:結果交出去。
    assert_eq!(
        parse_response(br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#, 3),
        Ok(Some(Ok(json!({"ok": true}))))
    );
    // 錯誤回應:Rpc。
    assert!(matches!(
        parse_response(
            br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"x"}}"#,
            3
        ),
        Ok(Some(Err(McpError::Rpc(_))))
    ));
    // 通知:略過。
    assert_eq!(
        parse_response(
            br#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#,
            3
        ),
        Ok(None)
    );
    // 別的 id、字串 id、server 的請求、不是 JSON、不是 UTF-8:都是違反協定。
    assert_eq!(
        parse_response(br#"{"jsonrpc":"2.0","id":4,"result":1}"#, 3),
        Err(McpError::Protocol("a response for another request".into()))
    );
    assert_eq!(
        parse_response(br#"{"jsonrpc":"2.0","id":"3","result":1}"#, 3),
        Err(McpError::Protocol("a response for another request".into()))
    );
    assert_eq!(
        parse_response(
            br#"{"jsonrpc":"2.0","id":"s","method":"sampling/createMessage"}"#,
            3
        ),
        Err(McpError::Protocol(
            "the server sent a request on a response stream".into()
        ))
    );
    assert!(matches!(
        parse_response(b"nope", 3),
        Err(McpError::Protocol(_))
    ));
    assert_eq!(
        parse_response(b"\xff", 3),
        Err(McpError::Protocol("not UTF-8".into()))
    );
}
