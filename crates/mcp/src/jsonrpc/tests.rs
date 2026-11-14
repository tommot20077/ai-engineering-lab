use proptest::prelude::*;
use serde_json::json;

use super::*;

fn invalid(reason: &str) -> Result<Incoming, ProtocolError> {
    Err(ProtocolError::Invalid(reason.into()))
}

// ---- 編碼 ----

#[test]
fn a_request_is_one_line_of_json() {
    assert_eq!(
        request(7, "tools/list", Some(json!({"cursor": "c1"}))),
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"cursor":"c1"}}"#
    );
    assert_eq!(
        request(1, "ping", None),
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#
    );
    // 參數裡的換行被跳脫成 `\n`:整則訊息還是一行。
    let text = request(2, "tools/call", Some(json!({"text": "a\nb"})));
    assert!(!text.contains('\n'), "{text}");
}

#[test]
fn a_notification_has_no_id() {
    assert_eq!(
        notification("notifications/initialized", None),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
    );
    assert_eq!(
        notification("notifications/cancelled", Some(json!({"requestId": 3}))),
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}}"#
    );
}

#[test]
fn an_error_response_answers_with_the_same_id() {
    assert_eq!(
        error_response(&Id::String("s-1".into()), METHOD_NOT_FOUND, "not supported"),
        r#"{"jsonrpc":"2.0","id":"s-1","error":{"code":-32601,"message":"not supported"}}"#
    );
    assert_eq!(
        error_response(&Id::Number(9), INTERNAL_ERROR, "x"),
        r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32603,"message":"x"}}"#
    );
}

// ---- 解析:合法的三種 ----

#[test]
fn responses_carry_a_result_or_an_error() {
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#),
        Ok(Incoming::Response {
            id: Id::Number(1),
            outcome: Ok(json!({"tools": []}))
        })
    );
    // `result` 是 null 也是成功(跟「沒有 result」不一樣)。
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":2,"result":null}"#),
        Ok(Incoming::Response {
            id: Id::Number(2),
            outcome: Ok(Value::Null)
        })
    );
    assert_eq!(
        parse(
            r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32602,"message":"bad","data":{"field":"x"}}}"#
        ),
        Ok(Incoming::Response {
            id: Id::String("a".into()),
            outcome: Err(RpcError {
                code: -32602,
                message: "bad".into(),
                data: Some(json!({"field": "x"}))
            })
        })
    );
}

#[test]
fn requests_and_notifications_from_the_server() {
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":"s-1","method":"sampling/createMessage","params":{"x":1}}"#),
        Ok(Incoming::Request {
            id: Id::String("s-1".into()),
            method: "sampling/createMessage".into(),
            params: Some(json!({"x": 1}))
        })
    );
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#),
        Ok(Incoming::Notification {
            method: "notifications/tools/list_changed".into(),
            params: None
        })
    );
}

#[test]
fn unknown_top_level_members_are_ignored() {
    // 規格之外的欄位不影響意思,略過(往後相容);意思不明確的情況才拒絕(下面的測試)。
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":1,"result":true,"extra":0}"#),
        Ok(Incoming::Response {
            id: Id::Number(1),
            outcome: Ok(json!(true))
        })
    );
}

// ---- 解析:不合法的,逐條規則 ----

#[test]
fn anything_but_a_json_object_is_rejected() {
    assert!(matches!(parse("not json"), Err(ProtocolError::NotJson(_))));
    assert!(matches!(parse(""), Err(ProtocolError::NotJson(_))));
    // 批次(陣列)在 MCP 裡不支援。
    assert_eq!(
        parse(r#"[{"jsonrpc":"2.0","id":1,"result":1}]"#),
        invalid("expected a JSON object, got an array")
    );
    assert_eq!(parse("42"), invalid("expected a JSON object, got a number"));
}

#[test]
fn the_version_must_be_2_0() {
    assert_eq!(
        parse(r#"{"id":1,"result":1}"#),
        invalid("`jsonrpc` must be \"2.0\"")
    );
    assert_eq!(
        parse(r#"{"jsonrpc":"1.0","id":1,"result":1}"#),
        invalid("`jsonrpc` must be \"2.0\"")
    );
    assert_eq!(
        parse(r#"{"jsonrpc":2.0,"id":1,"result":1}"#),
        invalid("`jsonrpc` must be \"2.0\"")
    );
}

#[test]
fn a_response_has_exactly_one_of_result_and_error() {
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"x"}}"#),
        invalid("a response has both `result` and `error`")
    );
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":1}"#),
        invalid("a response needs `result` or `error`")
    );
}

#[test]
fn a_request_has_no_result_or_error() {
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":1,"method":"x","result":1}"#),
        invalid("a request or notification must not have `result` or `error`")
    );
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","method":"x","error":{"code":1,"message":"x"}}"#),
        invalid("a request or notification must not have `result` or `error`")
    );
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","id":1,"method":7}"#),
        invalid("`method` must be a string")
    );
}

#[test]
fn ids_are_integers_or_strings() {
    for id in ["null", "1.5", "true", "[1]", r#"{"a":1}"#] {
        let text = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":1}}"#);
        assert_eq!(
            parse(&text),
            invalid("`id` must be an integer or a string"),
            "{id}"
        );
    }
    // 沒有 id 的回應:不知道是回哪個請求,也拒絕。
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"parse error"}}"#),
        invalid("a response needs an `id`")
    );
}

#[test]
fn a_malformed_error_object_is_rejected() {
    for error in [
        r#""oops""#,
        r#"{"message":"x"}"#,
        r#"{"code":"1","message":"x"}"#,
        r#"{"code":1}"#,
    ] {
        let text = format!(r#"{{"jsonrpc":"2.0","id":1,"error":{error}}}"#);
        assert_eq!(
            parse(&text),
            invalid("`error` must have an integer `code` and a string `message`"),
            "{error}"
        );
    }
}

#[test]
fn params_must_be_an_object_or_an_array() {
    // JSON-RPC 規定 params 是結構(物件或陣列)。
    assert_eq!(
        parse(r#"{"jsonrpc":"2.0","method":"x","params":"text"}"#),
        invalid("`params` must be an object or an array")
    );
    assert!(parse(r#"{"jsonrpc":"2.0","method":"x","params":[1]}"#).is_ok());
}

#[test]
fn an_rpc_error_prints_its_code_and_message() {
    let e = RpcError {
        code: -32601,
        message: "Method not found".into(),
        data: None,
    };
    assert_eq!(e.to_string(), "JSON-RPC error -32601: Method not found");
}

// ---- property tests ----

proptest! {
    /// 任意字串都只能回 Ok 或 Err,絕不 panic。
    #[test]
    fn never_panics(text in any::<String>()) {
        let _ = parse(&text);
    }

    /// 我們編碼的請求,用 server 的角度解析回來,內容一樣。
    #[test]
    fn a_request_round_trips(id in any::<i64>(), method in "[a-z/_]{1,20}", value in any::<i64>()) {
        let text = request(id, &method, Some(json!({"v": value})));
        prop_assert!(!text.contains('\n'));
        prop_assert_eq!(
            parse(&text),
            Ok(Incoming::Request { id: Id::Number(id), method, params: Some(json!({"v": value})) })
        );
    }
}

#[test]
fn the_standard_error_codes_are_the_ones_in_the_spec() {
    // 數值由 JSON-RPC 2.0 規格決定;寫錯的話,對方看到的錯誤種類就錯了(cargo-mutants:拿掉負號沒有測試失敗)。
    assert_eq!(
        [
            PARSE_ERROR,
            INVALID_REQUEST,
            METHOD_NOT_FOUND,
            INVALID_PARAMS,
            INTERNAL_ERROR
        ],
        [-32700, -32600, -32601, -32602, -32603]
    );
}
