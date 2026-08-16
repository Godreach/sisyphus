//! CSRF 面中间件（票 B2b-T2，ADR-0014）：cookie 会话的跨站请求防护。
//!
//! 挂在 `/api/v1` 受保护段、会话认证中间件（[`super::auth::require_auth`]）
//! 的内层——只对「已过认证且以 cookie 认证」的非安全方法请求（POST/PUT/
//! PATCH/DELETE）生效：
//!
//! - **Origin 头存在则须同源**（与 Host 比对）；否则看 **Sec-Fetch-Site**
//!   （须 `same-origin` / `same-site`）；不匹配或双头皆缺即拒（403，
//!   `CSRF_REJECTED`）——浏览器发跨站请求必带其一，合法浏览器请求天然
//!   通过，伪造不了。
//! - **Bearer 免疫**：携 `Authorization` 头的请求不经此检查——脚本与 CI
//!   走 PAT（Bearer 认证不依赖 cookie，无 CSRF 面，ADR-0014）。
//! - GET 等安全方法不拦；login/setup 在公开段不经认证中间件，天然不受
//!   影响（浏览器同源登录请求必带 Sec-Fetch-Site）。
//!
//! 裁决是纯函数（[`verdict`]，可全矩阵单测），中间件只做薄接线。

use axum::extract::Request;
use axum::http::{HeaderMap, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::auth::cookie_value;
use super::error::ApiError;
use crate::auth::SESSION_COOKIE_NAME;

/// CSRF 检查结论。
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// 放行（安全方法 / 非 cookie 认证面 / 同源凭证成立）。
    Pass,
    /// 拒绝（403 CSRF_REJECTED）。
    Reject,
}

/// 对单请求的 CSRF 裁决（纯函数；中间件的全部判定逻辑在此）。
fn verdict(method: &Method, headers: &HeaderMap) -> Verdict {
    // 非安全方法才拦：GET/HEAD/OPTIONS 无副作用，跨站读取由 SameSite=Lax
    // cookie 挡住顶级导航以外的携行。
    if !matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        return Verdict::Pass;
    }
    // 只管「以 cookie 认证」的请求：未携会话 cookie 的请求交给认证中间件
    // （401），不在此重复把关。
    if cookie_value(headers, SESSION_COOKIE_NAME).is_none() {
        return Verdict::Pass;
    }
    // Bearer 免疫：显式凭据（PAT/Agent token 面）不依赖 cookie，不存在
    // CSRF 面；cookie + Bearer 并存时按显式凭据对待。只认 Bearer scheme
    // —— 其它 Authorization 形态（Basic 等）不在免疫面，照常走同源校验。
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_bearer)
    {
        return Verdict::Pass;
    }

    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase);

    if let Some(origin) = origin {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        if same_origin(origin, host) {
            Verdict::Pass
        } else {
            Verdict::Reject
        }
    } else if let Some(site) = site {
        // same-site 放行：子域之间的 cookie 携行在 SameSite=Lax 语义内本就
        // 允许，同站发起的请求不算跨站攻击面。
        if site == "same-origin" || site == "same-site" {
            Verdict::Pass
        } else {
            Verdict::Reject
        }
    } else {
        // 双头皆缺：浏览器必带其一，缺失即非浏览器（或刻意剥离），按
        // cookie 认证的非浏览器请求一律拒——脚本请走 PAT。
        Verdict::Reject
    }
}

/// Origin 与请求 Host 是否同源。
///
/// 只比对 host[:port]（大小写归一、scheme 默认端口剥除）：Server 不自知
/// 是否身处 TLS 终止的反代之后，比对 scheme 会把合法的 `https://host`
/// （反代后）误杀；Host 缺失（无法建立同源基准）视为不同源，失败关闭。
fn same_origin(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let origin_authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    normalize_authority(origin_authority, default_port) == normalize_authority(host, None)
}

/// 归一化 host[:port]：小写；显式端口恰为该 scheme 默认端口时剥除
/// （`http://h:80` 与 `Host: h` 同源）。
fn normalize_authority(authority: &str, default_port: Option<u16>) -> String {
    let lower = authority.trim().to_ascii_lowercase();
    match (lower.rsplit_once(':'), default_port) {
        (Some((host, port)), Some(default)) if port == default.to_string() => host.to_string(),
        _ => lower,
    }
}

/// Authorization 头是否为 Bearer 凭据（scheme 大小写不敏感，RFC 7235）。
fn is_bearer(value: &str) -> bool {
    value
        .split_ascii_whitespace()
        .next()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer"))
}

/// CSRF 中间件（挂认证中间件内层）：裁决拒绝即 403 统一错误形态。
pub async fn csrf_protect(req: Request, next: Next) -> Response {
    if verdict(req.method(), req.headers()) == Verdict::Reject {
        return ApiError::csrf_rejected().into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便捷构造头表。
    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_static(name),
                value.parse().expect("合法头值"),
            );
        }
        map
    }

    #[test]
    fn safe_methods_and_non_cookie_requests_pass() {
        let cookie = headers(&[("cookie", "sisyphus_session=abc")]);
        for method in ["GET", "HEAD", "OPTIONS"] {
            assert_eq!(
                verdict(&Method::from_bytes(method.as_bytes()).unwrap(), &cookie),
                Verdict::Pass,
                "{method} 是安全方法，不拦"
            );
        }

        // 未携会话 cookie 的非安全方法：交给认证中间件（401），不在此拦。
        let no_cookie = headers(&[]);
        assert_eq!(verdict(&Method::POST, &no_cookie), Verdict::Pass);

        // 携其它 cookie 但无会话 cookie：同样不是 cookie 认证面。
        let other_cookie = headers(&[("cookie", "theme=dark")]);
        assert_eq!(verdict(&Method::PUT, &other_cookie), Verdict::Pass);
    }

    #[test]
    fn bearer_credentials_are_immune() {
        // 即便 Origin 跨源 / 双头皆缺，Bearer 面不经此检查（scheme 大小写
        // 不敏感）。
        for auth in ["Bearer sis_whatever", "bearer sis_lower"] {
            let bearer = headers(&[("cookie", "sisyphus_session=abc"), ("authorization", auth)]);
            assert_eq!(verdict(&Method::POST, &bearer), Verdict::Pass, "{auth}");
        }

        // 非 Bearer 的 Authorization（如 Basic）不在免疫面：cookie 认证的
        // 请求照常走同源校验。
        let basic = headers(&[
            ("cookie", "sisyphus_session=abc"),
            ("authorization", "Basic dXNlcjpwYXNz"),
            ("origin", "https://evil.example"),
            ("host", "ci.local"),
        ]);
        assert_eq!(verdict(&Method::POST, &basic), Verdict::Reject);
    }

    #[test]
    fn origin_same_passes_and_cross_origin_rejects() {
        let same = headers(&[
            ("cookie", "sisyphus_session=abc"),
            ("origin", "http://ci.local"),
            ("host", "ci.local"),
        ]);
        assert_eq!(verdict(&Method::POST, &same), Verdict::Pass);

        let cross = headers(&[
            ("cookie", "sisyphus_session=abc"),
            ("origin", "https://evil.example"),
            ("host", "ci.local"),
        ]);
        assert_eq!(verdict(&Method::PUT, &cross), Verdict::Reject);

        // Origin 存在但 Host 缺失：无法建立同源基准，失败关闭。
        let no_host = headers(&[
            ("cookie", "sisyphus_session=abc"),
            ("origin", "http://ci.local"),
        ]);
        assert_eq!(verdict(&Method::PATCH, &no_host), Verdict::Reject);

        // Origin "null"（沙箱 iframe 等）不是同源。
        let null_origin = headers(&[
            ("cookie", "sisyphus_session=abc"),
            ("origin", "null"),
            ("host", "ci.local"),
        ]);
        assert_eq!(verdict(&Method::DELETE, &null_origin), Verdict::Reject);
    }

    #[test]
    fn sec_fetch_site_decides_when_origin_absent() {
        for site in ["same-origin", "same-site", "Same-Origin"] {
            let headers = headers(&[("cookie", "sisyphus_session=abc"), ("sec-fetch-site", site)]);
            assert_eq!(verdict(&Method::POST, &headers), Verdict::Pass, "{site}");
        }
        for site in ["cross-site", "none"] {
            let headers = headers(&[("cookie", "sisyphus_session=abc"), ("sec-fetch-site", site)]);
            assert_eq!(verdict(&Method::POST, &headers), Verdict::Reject, "{site}");
        }

        // 双头皆缺：拒（浏览器必带其一；脚本走 PAT）。
        let bare = headers(&[("cookie", "sisyphus_session=abc")]);
        assert_eq!(verdict(&Method::POST, &bare), Verdict::Reject);
    }

    #[test]
    fn same_origin_normalizes_default_ports_case_and_paths() {
        assert!(same_origin("http://ci.local", Some("ci.local")));
        assert!(same_origin("http://CI.local/", Some("ci.local")));
        assert!(same_origin("http://ci.local:80/x", Some("ci.local")));
        assert!(same_origin("https://ci.local:443", Some("ci.local")));
        assert!(same_origin(
            "http://ci.local:8080/app",
            Some("ci.local:8080")
        ));
        assert!(same_origin("https://ci.local", Some("ci.local")));

        // 端口不匹配 / 不同主机 / 非法 Origin 串：不同源。
        assert!(!same_origin("http://ci.local", Some("ci.local:8080")));
        assert!(!same_origin("http://ci.local:8080", Some("ci.local")));
        assert!(!same_origin("http://other.local", Some("ci.local")));
        assert!(!same_origin("ci.local", Some("ci.local")));
        assert!(!same_origin("http://ci.local", None));
    }
}
