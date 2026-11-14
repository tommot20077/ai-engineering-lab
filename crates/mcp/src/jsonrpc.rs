//! JSON-RPC 2.0 的訊息(design-part2 §4)。
//!
//! 一則訊息是下面三種之一:
//!
//! ```text
//! 請求    {"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {...}}   要回應
//! 通知    {"jsonrpc": "2.0", "method": "notifications/initialized"}              沒有 id,不回應
//! 回應    {"jsonrpc": "2.0", "id": 1, "result": {...}}  或  "error": {...}       用 id 對回請求
//! ```
//!
//! 送出的訊息由我們組,型別簡單;收到的訊息是不可信輸入,解析時檢查每一條規則(fail-closed)。

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 的版本字串,每則訊息都要有。
pub const VERSION: &str = "2.0";

// 標準的錯誤碼。
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// 請求的 id:數字或字串。我們送出的永遠是數字;server 送來的請求可能用字串。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

/// 錯誤回應裡的 `error`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// 收到的一則訊息。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// 回應我們送出的請求。`Ok` 是 `result`(可能是 `null`),`Err` 是 `error`。
    Response {
        id: Id,
        outcome: Result<Value, RpcError>,
    },
    /// server 對 client 的請求(sampling 之類)。我們不支援,要回 `METHOD_NOT_FOUND`。
    Request {
        id: Id,
        method: String,
        params: Option<Value>,
    },
    /// 通知:不用回。
    Notification {
        method: String,
        params: Option<Value>,
    },
}

/// 收到的東西不是合法的 JSON-RPC 2.0 訊息。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("not JSON: {0}")]
    NotJson(String),
    #[error("not a JSON-RPC 2.0 message: {0}")]
    Invalid(String),
}

/// 送出的訊息。三種訊息共用一個 struct,沒有的欄位不輸出(`skip_serializing_if`)。
// 欄位的宣告順序就是 JSON 的順序:`jsonrpc` 永遠在最前面,讀 log 比較順。
#[derive(Serialize)]
struct Outgoing<'a> {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

impl Outgoing<'_> {
    /// 一行 JSON。serde_json 的緊湊格式不會輸出換行:字串裡的換行會被跳脫成 `\n`。
    fn encode(&self) -> String {
        // 這些型別一定序列化得出來(key 都是字串);萬一失敗,空字串在對方那邊是解析錯誤,不會被誤解。
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// 編碼一個請求,單一行(stdio 傳輸一行一則訊息)。
pub fn request(id: i64, method: &str, params: Option<Value>) -> String {
    Outgoing {
        jsonrpc: VERSION,
        id: Some(&Id::Number(id)),
        method: Some(method),
        params,
        error: None,
    }
    .encode()
}

/// 編碼一個通知。
pub fn notification(method: &str, params: Option<Value>) -> String {
    Outgoing {
        jsonrpc: VERSION,
        id: None,
        method: Some(method),
        params,
        error: None,
    }
    .encode()
}

/// 編碼一個錯誤回應(回給 server 送來、我們不支援的請求)。
pub fn error_response(id: &Id, code: i64, message: &str) -> String {
    Outgoing {
        jsonrpc: VERSION,
        id: Some(id),
        method: None,
        params: None,
        error: Some(RpcError {
            code,
            message: message.to_owned(),
            data: None,
        }),
    }
    .encode()
}

/// 解析收到的一則訊息。
///
/// 為什麼不用 `#[derive(Deserialize)]` 一次解析:規則是「`result` 與 `error` 恰好一個」「有 `method` 就不能有 `result`」,
/// serde 的屬性表達不了;而且 `"result": null` 與「沒有 `result`」意思不同,`Option<Value>` 會把兩者都變成 `None`。
/// 所以先解析成一般的 JSON 物件,再逐條檢查,通過了才變成 `Incoming`(parse, don't validate)。
pub fn parse(text: &str) -> Result<Incoming, ProtocolError> {
    let invalid = |reason: &str| ProtocolError::Invalid(reason.to_owned());
    let value: Value =
        serde_json::from_str(text).map_err(|e| ProtocolError::NotJson(e.to_string()))?;
    let mut map = match value {
        Value::Object(map) => map,
        other => {
            return Err(ProtocolError::Invalid(format!(
                "expected a JSON object, got {}",
                kind(&other)
            )));
        }
    };
    if map.get("jsonrpc") != Some(&Value::from(VERSION)) {
        return Err(invalid("`jsonrpc` must be \"2.0\""));
    }
    // `transpose`:`Option<Result<Id, _>>` 換成 `Result<Option<Id>, _>`,才能用 `?`。
    // 「有 id 但格式不對」是錯誤,「沒有 id」不是(通知本來就沒有)。
    let id = map.remove("id").map(parse_id).transpose()?;
    let params = match map.remove("params") {
        None => None,
        Some(params @ (Value::Object(_) | Value::Array(_))) => Some(params),
        Some(_) => return Err(invalid("`params` must be an object or an array")),
    };
    let result = map.remove("result");
    let error = map.remove("error");

    match map.remove("method") {
        Some(Value::String(method)) => {
            if result.is_some() || error.is_some() {
                return Err(invalid(
                    "a request or notification must not have `result` or `error`",
                ));
            }
            Ok(match id {
                Some(id) => Incoming::Request { id, method, params },
                None => Incoming::Notification { method, params },
            })
        }
        Some(_) => Err(invalid("`method` must be a string")),
        None => {
            let id = id.ok_or_else(|| invalid("a response needs an `id`"))?;
            let outcome = match (result, error) {
                (Some(_), Some(_)) => {
                    return Err(invalid("a response has both `result` and `error`"));
                }
                (Some(result), None) => Ok(result),
                (None, Some(error)) => {
                    Err(serde_json::from_value::<RpcError>(error).map_err(|_| {
                        invalid("`error` must have an integer `code` and a string `message`")
                    })?)
                }
                (None, None) => return Err(invalid("a response needs `result` or `error`")),
            };
            Ok(Incoming::Response { id, outcome })
        }
    }
}

/// 整數或字串。`1.5`、`null`、物件都不行(規格不建議 null 與小數,我們直接拒絕)。
fn parse_id(value: Value) -> Result<Id, ProtocolError> {
    match value {
        Value::Number(n) => n.as_i64().map(Id::Number),
        Value::String(s) => Some(Id::String(s)),
        _ => None,
    }
    .ok_or_else(|| ProtocolError::Invalid("`id` must be an integer or a string".into()))
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests;
