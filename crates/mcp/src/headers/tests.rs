use serde_json::json;

use super::*;

fn header(name: &str, path: &[&str]) -> ParamHeader {
    ParamHeader {
        name: name.into(),
        path: path.iter().map(|p| (*p).to_owned()).collect(),
    }
}

#[test]
fn annotations_on_reachable_properties_are_collected() {
    // 規格的例子,加上一個巢狀的物件屬性(每一步都是 `properties`,所以可達)。
    let schema = json!({
        "type": "object",
        "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "query": {"type": "string"},
            "options": {"type": "object", "properties": {
                "dry_run": {"type": "boolean", "x-mcp-header": "Dry-Run"}
            }}
        }
    });
    let mut headers = param_headers(&schema).unwrap();
    headers.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        headers,
        [
            header("Dry-Run", &["options", "dry_run"]),
            header("Region", &["region"])
        ]
    );
    assert_eq!(param_headers(&json!({"type": "object"})), Ok(vec![]));
}

#[test]
fn annotations_anywhere_else_make_the_tool_invalid() {
    let stray = Err(
        "`x-mcp-header` is only allowed on properties reachable through `properties`".to_owned(),
    );
    for schema in [
        // 在陣列的 items 裡。
        json!({"type": "object", "properties": {"list": {"type": "array", "items": {"type": "string", "x-mcp-header": "X"}}}}),
        // 在 oneOf 裡。
        json!({"type": "object", "oneOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
        // 在 $defs 裡(要經過 $ref 才到得了)。
        json!({"type": "object", "$defs": {"d": {"type": "string", "x-mcp-header": "D"}}}),
        // 根本身。
        json!({"type": "object", "x-mcp-header": "Root"}),
    ] {
        assert_eq!(param_headers(&schema), stray, "{schema}");
    }
}

#[test]
fn a_parameter_named_x_mcp_header_is_just_a_parameter() {
    // `properties` 底下的 key 是參數名稱,不是關鍵字。
    let schema = json!({"type": "object", "properties": {"x-mcp-header": {"type": "string"}}});
    assert_eq!(param_headers(&schema), Ok(vec![]));
}

#[test]
fn annotation_values_must_be_tokens() {
    let token = Err("`x-mcp-header` must be a non-empty HTTP token".to_owned());
    for value in [
        json!(""),
        json!("Has Space"),
        json!("a:b"),
        json!("crlf\r\n"),
        json!(5),
    ] {
        let schema = json!({"type": "object", "properties": {"p": {"type": "string", "x-mcp-header": value}}});
        assert_eq!(param_headers(&schema), token, "{value}");
    }
    // tchar 裡的符號都可以。
    let schema = json!({"type": "object", "properties": {"p": {"type": "string", "x-mcp-header": "a!#$%&'*+-.^_`|~9"}}});
    assert!(param_headers(&schema).is_ok());
}

#[test]
fn annotation_values_are_unique_ignoring_case() {
    let schema = json!({"type": "object", "properties": {
        "a": {"type": "string", "x-mcp-header": "Region"},
        "b": {"type": "string", "x-mcp-header": "region"}
    }});
    let err = param_headers(&schema).unwrap_err();
    assert!(err.ends_with("is used twice"), "{err}");
}

#[test]
fn only_primitive_parameters_can_be_headers() {
    for kind in ["number", "object", "array", "null"] {
        let schema =
            json!({"type": "object", "properties": {"p": {"type": kind, "x-mcp-header": "P"}}});
        assert_eq!(
            param_headers(&schema),
            Err("`x-mcp-header` `P` must be on a string, integer or boolean parameter".to_owned()),
            "{kind}"
        );
    }
    // 沒寫 type 也不行(不能保證是基本型別)。
    let schema = json!({"type": "object", "properties": {"p": {"x-mcp-header": "P"}}});
    assert!(param_headers(&schema).is_err());
    for kind in ["string", "integer", "boolean"] {
        let schema =
            json!({"type": "object", "properties": {"p": {"type": kind, "x-mcp-header": "P"}}});
        assert!(param_headers(&schema).is_ok(), "{kind}");
    }
}

#[test]
fn values_are_read_at_the_exact_path() {
    let headers = [
        header("Region", &["region"]),
        header("Dry-Run", &["options", "dry_run"]),
        header("Count", &["count"]),
        header("Missing", &["nope"]),
        header("Ratio", &["ratio"]),
    ];
    let arguments = json!({
        "region": "us-west1", "options": {"dry_run": true}, "count": -7, "ratio": 0.5
    });
    let Value::Object(arguments) = arguments else {
        unreachable!()
    };
    // 沒有值的不送;小數(number)不是規格允許的型別,也不送。
    assert_eq!(
        param_values(&headers, &arguments),
        [
            ("Region".to_owned(), "us-west1".to_owned()),
            ("Dry-Run".to_owned(), "true".to_owned()),
            ("Count".to_owned(), "-7".to_owned()),
        ]
    );
}

#[test]
fn values_are_encoded_like_the_spec_examples() {
    // 規格 Value Encoding 表格的五個例子。
    assert_eq!(encode_value("us-west1"), "us-west1");
    assert_eq!(
        encode_value("Hello, 世界"),
        "=?base64?SGVsbG8sIOS4lueVjA==?="
    );
    assert_eq!(encode_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
    assert_eq!(encode_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
    assert_eq!(
        encode_value("=?base64?literal?="),
        "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
    );
    // 中間的空白、tab 可以原樣;只有開頭或只有結尾像編碼標記的,不算。
    assert_eq!(encode_value("a b\tc"), "a b\tc");
    assert_eq!(encode_value("=?base64?abc"), "=?base64?abc");
    assert_eq!(encode_value("abc?="), "abc?=");
    assert_eq!(encode_value("\u{7f}"), "=?base64?fw==?=");
}

#[test]
fn base64_matches_rfc_4648() {
    // RFC 4648 §10 的測試向量:長度 0~6,涵蓋補 0、1、2 個 `=`。
    for (input, output) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), output, "{input}");
    }
    // 每個字母表的位置都出現:0x00..0xff 的 base64 有固定的答案,用長度與頭尾檢查。
    let all: Vec<u8> = (0..=255).collect();
    let encoded = base64(&all);
    assert_eq!(encoded.len(), 344);
    assert!(
        encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4v")
    );
    assert!(encoded.ends_with("+/w=="), "{encoded}");
}
