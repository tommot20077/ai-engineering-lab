//! HTTP header 的規則(規格 2026-07-28 Streamable HTTP「Request Metadata」)。
//!
//! 兩件事:
//! - **`x-mcp-header`**:工具的 `inputSchema` 裡,參數可以標上 `"x-mcp-header": "Region"`,client 呼叫時要把那個參數的值
//!   複製到 `Mcp-Param-Region` header(讓 gateway 不必解析 body 就能路由)。標註不合法的工具要排除。
//! - **header 值的編碼**:不是單純可見 ASCII 的值(中文、控制字元、頭尾空白)改成 `=?base64?…?=`。
//!
//! 規則跟傳輸無關:stdio 用不到 header,但同一個工具定義在兩種傳輸下應該一樣被接受或拒絕,所以 client 一律檢查。

use serde_json::{Map, Value};

/// 一個要複製到 header 的參數:`path` 是從根經過一連串 `properties` 走到它的屬性名稱。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParamHeader {
    /// `Mcp-Param-` 後面的部分。
    pub name: String,
    pub path: Vec<String>,
}

/// 找出 schema 裡所有的 `x-mcp-header`,並檢查規格的每一條限制。任何一條不符合,整個工具都不能用(`Err` 是原因)。
pub(crate) fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut found = Vec::new();
    let mut stray = false;
    scan(schema, Some(&mut Vec::new()), &mut found, &mut stray);
    if stray {
        return Err(
            "`x-mcp-header` is only allowed on properties reachable through `properties`".into(),
        );
    }
    let mut seen = std::collections::HashSet::new();
    let mut headers = Vec::with_capacity(found.len());
    for (path, annotation, property) in found {
        let Some(name) = annotation.as_str().filter(|n| is_token(n)) else {
            return Err("`x-mcp-header` must be a non-empty HTTP token".into());
        };
        // header 名稱不分大小寫:`Region` 和 `region` 是同一個 header。
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!("`x-mcp-header` `{name}` is used twice"));
        }
        let primitive = matches!(
            property.get("type").and_then(Value::as_str),
            Some("string" | "integer" | "boolean")
        );
        if !primitive {
            return Err(format!(
                "`x-mcp-header` `{name}` must be on a string, integer or boolean parameter"
            ));
        }
        headers.push(ParamHeader {
            name: name.to_owned(),
            path,
        });
    }
    Ok(headers)
}

/// 走過整個 schema。`path` 是 `Some` 表示「從根只經過 `properties` 走到這裡」(規格說的 statically reachable)。
/// 這種位置上的 `x-mcp-header` 收進 `found`;其他位置(`items`、`oneOf`、`$defs`……,或根本身)出現就是 `stray`。
fn scan<'a>(
    schema: &'a Value,
    path: Option<&mut Vec<String>>,
    found: &mut Vec<(Vec<String>, &'a Value, &'a Value)>,
    stray: &mut bool,
) {
    match schema {
        Value::Object(map) => {
            let mut path = path;
            if let Some(annotation) = map.get("x-mcp-header") {
                match path.as_deref() {
                    Some(p) if !p.is_empty() => found.push((p.clone(), annotation, schema)),
                    _ => *stray = true,
                }
            }
            for (key, value) in map {
                match (key.as_str(), value, path.as_deref_mut()) {
                    // `properties` 底下的 key 是參數名稱,不是 schema 關鍵字:逐一往下走,路徑加上名稱。
                    ("properties", Value::Object(properties), Some(p)) => {
                        for (name, sub) in properties {
                            p.push(name.clone());
                            scan(sub, Some(p), found, stray);
                            p.pop();
                        }
                    }
                    // 不在可達路徑上的 `properties`:裡面的 schema 照樣要找,只是都算不可達。
                    ("properties", Value::Object(properties), None) => {
                        for sub in properties.values() {
                            scan(sub, None, found, stray);
                        }
                    }
                    ("x-mcp-header", _, _) => {}
                    _ => scan(value, None, found, stray),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                scan(item, None, found, stray);
            }
        }
        _ => {}
    }
}

/// RFC 9110 的 token:`1*tchar`。
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// 依 `param_headers` 的結果,從呼叫參數取出 header。路徑上沒有值、或值不是字串 / 整數 / 布林,就不送這個 header
/// (參數本身已經用 schema 驗證過)。
pub(crate) fn param_values(
    headers: &[ParamHeader],
    arguments: &Map<String, Value>,
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|header| {
            let (first, rest) = header.path.split_first()?;
            let value = rest
                .iter()
                .try_fold(arguments.get(first)?, |value, key| value.get(key))?;
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((header.name.clone(), encode_value(&text)))
        })
        .collect()
}

/// header 值的編碼:可見 ASCII(加空白與 tab)、頭尾沒有空白、而且不像編碼過的值 → 原樣;否則 `=?base64?…?=`。
pub(crate) fn encode_value(value: &str) -> String {
    let plain = value.bytes().all(|b| matches!(b, b'\t' | 0x20..=0x7e))
        && value.trim() == value
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    if plain {
        value.to_owned()
    } else {
        format!("=?base64?{}?=", base64(value.as_bytes()))
    }
}

/// 標準 Base64(RFC 4648,有 `=` 補齊)。依賴樹裡沒有 base64 crate,演算法很小,自己寫。
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    // 每 3 個位元組(24 位元)變成 4 個字元(各 6 位元)。`chunks(3)` 的最後一塊可能只有 1 或 2 個位元組。
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        // 一塊有幾個位元組,就輸出幾 + 1 個字元,其餘補 `=`。
        for i in 0..4 {
            if i <= chunk.len() {
                let index = (n >> (18 - 6 * i)) & 0x3f;
                out.push(char::from(
                    ALPHABET.get(index as usize).copied().unwrap_or(b'A'),
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
