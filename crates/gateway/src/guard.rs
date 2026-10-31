//! 請求進到 handler 之前的兩道檢查(design-part2.md §18、D-024)。
//!
//! 「只聽本機」不等於「只有你連得到」:你瀏覽的網頁、你裝的其他程式都能打 `127.0.0.1`。
//! - **Host 檢查**:擋 DNS rebinding。攻擊者的網頁讓瀏覽器以為 `evil.example` 就是 127.0.0.1,
//!   但瀏覽器送出的 `Host` 仍然是 `evil.example`,在這裡被擋下。
//! - **token**:擋本機上的其他程式,以及其他網頁。
//!
//! Java 對照:Servlet `Filter` / Spring Security 的 filter chain。axum 的 middleware 就是一個
//! async 函式:檢查完呼叫 `next.run(request)` 交給下一層,不通過就直接回錯誤。

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;

use crate::api::AppState;
use crate::error::ApiError;

/// `Host` header 必須在允許清單裡。
pub(crate) async fn require_allowed_host(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    // 主機名稱不分大小寫(RFC 4343):`LOCALHOST:8787` 跟 `localhost:8787` 是同一個。
    let allowed = host.is_some_and(|host| {
        state
            .allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    });
    if !allowed {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_host",
            "this Host is not allowed; see server.allowed_hosts in gateway.toml",
        ));
    }
    Ok(next.run(request).await)
}

/// `Authorization: Bearer <token>` 必須正確。
pub(crate) async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    // 固定時間比較:一般的 `==` 遇到第一個不同的字元就停,攻擊者量回應時間就能一個字一個字猜出 token。
    // `ct_eq` 不管哪裡不同都比完整串。長度不同會立刻回 false(只洩漏長度,而 token 長度本來就是公開的 64 字)。
    // `bool::from`:`ct_eq` 回傳的 `Choice` 刻意不是 bool,避免 compiler 把它最佳化成會提早結束的比較。
    // Java 對照:`MessageDigest.isEqual()`,而不是 `String.equals()`。
    let valid = presented.is_some_and(|presented| {
        bool::from(
            presented
                .as_bytes()
                .ct_eq(state.token.expose_secret().as_bytes()),
        )
    });
    if !valid {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid token; send Authorization: Bearer <GATEWAY_TOKEN>",
        ));
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};

    use crate::api::router;
    use crate::testing::{HOST, Scripted, TOKEN, answer, chat_body, send_as, state};

    async fn chat_as(host: Option<&str>, token: Option<&str>) -> (crate::testing::Reply, usize) {
        let provider = Scripted::new(answer());
        let app = router(state(provider.clone()));
        let reply = send_as(app, "POST", "/v1/chat", Some(chat_body(64)), host, token).await;
        (reply, provider.calls())
    }

    #[tokio::test]
    async fn the_right_host_and_token_get_through() {
        let (reply, calls) = chat_as(Some(HOST), Some(TOKEN)).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn a_missing_token_is_401_and_the_provider_is_never_called() {
        let (reply, calls) = chat_as(Some(HOST), None).await;

        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.json()["error"]["kind"], "unauthorized");
        // RFC 7235:401 一定要告訴對方該用哪種驗證方式。
        assert_eq!(reply.headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn a_wrong_token_is_401() {
        for wrong in ["test-tokem", "test-token-but-longer", "", "TEST-TOKEN"] {
            let (reply, calls) = chat_as(Some(HOST), Some(wrong)).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{wrong:?}");
            assert_eq!(calls, 0);
        }
    }

    #[tokio::test]
    async fn a_token_must_come_as_bearer() {
        let provider = Scripted::new(answer());
        let app = router(state(provider));
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/v1/status")
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, format!("Basic {TOKEN}"))
            .body(axum::body::Body::empty())
            .unwrap();

        let response = tower::ServiceExt::oneshot(app, request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_foreign_host_is_403_even_with_a_valid_token() {
        // DNS rebinding:token 再對,Host 不對也不行。而且 Host 檢查在 token 之前,錯誤是 403 不是 401。
        let (reply, calls) = chat_as(Some("evil.example:8787"), Some(TOKEN)).await;

        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.json()["error"]["kind"], "invalid_host");
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn the_host_is_checked_before_the_token() {
        // Host 錯、也沒帶 token:回 403 而不是 401,證明 Host 檢查在外層、先執行。
        // (只用「Host 錯 + token 對」測不出順序:兩種順序都會回 403。)
        let (reply, _) = chat_as(Some("evil.example:8787"), None).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_missing_host_is_403() {
        let (reply, _) = chat_as(None, Some(TOKEN)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn host_names_are_compared_without_case() {
        // 主機名稱不分大小寫(RFC 4343)。
        let (reply, _) = chat_as(Some("LOCALHOST:8787"), Some(TOKEN)).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }

    #[tokio::test]
    async fn health_needs_neither_a_token_nor_an_allowed_host() {
        let provider = Scripted::new(answer());
        let app = router(state(provider));

        let reply = send_as(app, "GET", "/health", None, Some("10.0.0.7:8787"), None).await;

        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["status"], "ok");
    }

    #[tokio::test]
    async fn status_and_unknown_paths_need_a_token_too() {
        // 沒帶 token 的人連「這個路徑存不存在」都不該知道。
        for path in ["/v1/status", "/v1/models", "/v1/nope"] {
            let provider = Scripted::new(answer());
            let app = router(state(provider));
            let reply = send_as(app, "GET", path, None, Some(HOST), None).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        }
    }
}
