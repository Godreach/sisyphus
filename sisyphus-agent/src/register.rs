//! Agent 注册引导（票 #57，Spec B3 §7）：凭一次性注册码向 Server REST 端点
//! 换长期 per-Agent token，落盘 `<data>/token`（0600）。
//!
//! - HTTP 面与升级包下载同面（reqwest，ADR-0007：产物/升级/注册都走
//!   axum REST 面不走 gRPC 流）；注册是「Agent 主动外连」——与通道同向，
//!   天然适合引导期。
//! - 兑码即换新（Server 侧语义）：库里只存 token 哈希、旧明文不可找回，
//!   Server 在兑码时重新签发并吊销旧 token。本模块只消费响应里的 token
//!   并落盘；后续启动读 token 直连，不再需要注册码。
//! - 失败（无效/已用/过期/停用/网络不可达）明确报错退出——注册是引导
//!   步骤，不静默降级（没 token 连不上通道，退避重试没有意义）。
//!
//! 可测性：HTTP 调用收在 [`register`]（注入 reqwest client + 显式
//! api_url/name/reg_key），落盘收在 [`persist_token`]（注入数据目录）——
//! dev-deps 用极简 HTTP stub 驱动真 client 验证请求形态与错误路径，不依赖
//! server crate。

use std::fmt;
use std::path::Path;

/// 注册端点路径（挂 `/api/v1/` 下；与 Server 侧 `api::agents::register` 契约）。
pub const REGISTER_ENDPOINT: &str = "/api/v1/agent/register";

/// 注册错误：明确的失败类别（bin 据此打印人读报错并退出）。
#[derive(Debug)]
pub enum RegisterError {
    /// HTTP 传输失败（网络不可达、TLS 失败等）。
    Network(String),
    /// 端点返回非 200（注册码无效/已用/过期/停用/服务端错误）。
    Rejected {
        /// HTTP 状态码。
        status: u16,
        /// 服务端错误体（统一 JSON 形态的 message；缺省取状态码文本）。
        message: String,
    },
    /// 200 但响应体不是预期形态（token 缺失/非法）。
    Response(String),
    /// token 落盘失败。
    Io(std::io::Error),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::Network(e) => write!(f, "注册请求失败（网络/传输）：{e}"),
            RegisterError::Rejected { status, message } => {
                write!(f, "注册被拒绝（HTTP {status}）：{message}")
            }
            RegisterError::Response(e) => write!(f, "注册响应形态非法：{e}"),
            RegisterError::Io(e) => write!(f, "token 落盘失败：{e}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// 拼注册端点 URL：`{api_url}` 去尾斜杠 + 端点路径。
pub fn register_url(api_url: &str) -> String {
    format!("{}{REGISTER_ENDPOINT}", api_url.trim_end_matches('/'))
}

/// 注册响应体（与 Server `RegisterAgentResponse` 同构；本模块只消费 token）。
#[derive(serde::Deserialize)]
struct RegisterResponse {
    token: String,
}

/// 兑码换 token：POST `{api_url}/api/v1/agent/register`，body
/// `{"name": "<agent 名>", "register_code": "<reg_key>"}`，返回 Server
/// 签发的 per-Agent token（`sisa_` 族）。非 200 明确报错（读统一 JSON
/// 错误体里的 message，缺省落状态码文本）。
pub async fn register(
    client: &reqwest::Client,
    api_url: &str,
    name: &str,
    reg_key: &str,
) -> Result<String, RegisterError> {
    let url = register_url(api_url);
    let body = serde_json::json!({
        "name": name,
        "register_code": reg_key,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| RegisterError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let status_text = resp.status().to_string();
        let message = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| format!("HTTP {status_text}"));
        return Err(RegisterError::Rejected { status, message });
    }
    let parsed: RegisterResponse = resp
        .json()
        .await
        .map_err(|e| RegisterError::Response(e.to_string()))?;
    if parsed.token.is_empty() || !parsed.token.starts_with("sisa_") {
        return Err(RegisterError::Response("token 缺失或形态非法".into()));
    }
    Ok(parsed.token)
}

/// token 落盘 `<data>/token`（Unix 0600——per-Agent 长期凭据，与 Server
/// 主密钥文件同纪律；Windows 无 POSIX 权限位、尽力而为）。原子写：先写
/// 同目录临时文件再 rename，避免半截文件被下次启动读到；rename 失败时
/// 清理临时文件（不留半成品）。
pub fn persist_token(data_dir: &Path, token: &str) -> Result<(), RegisterError> {
    use std::io::Write;

    let path = data_dir.join(crate::config::TOKEN_FILE_NAME);
    let tmp = data_dir.join(format!("{}.tmp", crate::config::TOKEN_FILE_NAME));
    let mut file = std::fs::File::create(&tmp).map_err(RegisterError::Io)?;
    file.write_all(token.as_bytes())
        .map_err(RegisterError::Io)?;
    file.write_all(b"\n").map_err(RegisterError::Io)?;
    // Unix 0600（仅属主读写）；Windows 无 POSIX 权限位，尽力而为。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(RegisterError::Io)?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RegisterError::Io(e)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_url_trims_trailing_slash() {
        assert_eq!(
            register_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/api/v1/agent/register"
        );
        assert_eq!(
            register_url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080/api/v1/agent/register"
        );
    }
}
