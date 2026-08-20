//! SCM 探测 / 测试连接 / 分支枚举 / 凭据管理端点（票 B5-T3，ADR-0016）。
//!
//! - **测试连接（创建期，ad-hoc）**：`POST /projects/scm-probe`（全局 admin）——
//!   body 带 scm_type + scm_url + 可选 username/password，`git ls-remote` /
//!   `svn info` 探测返回 head（git sha / svn revision），不落库、不阻塞保存；
//!   失败可读错误（凭据错误不回显凭据）。
//! - **分支枚举（创建期预填）**：`POST /projects/scm-branches`（全局 admin，
//!   git only）——`ls-remote --heads` 列分支 + `--symref HEAD` 解析默认分支，
//!   供新建项目默认分支预填（ADR-0016）。
//! - **测试连接（既有项目，存储凭据）**：`POST /projects/{name}/test-connection`
//!   （项目 admin）——解密项目存储凭据探测 head（项目设置「测试连接」）。
//! - **凭据管理**：`PUT /projects/{name}/scm-credential`（项目 admin）——
//!   username + password 加密落库（空=清），审计 `scm_credential_set`。
//!
//! 凭据永不上命令行/URL（ADR-0015/0016）：经 [`crate::scm`] ASKPASS/stdin 递送；
//! 错误消息不含凭据（[`ProbeError::message`] 已脱敏）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::{RequireAdmin, RequireGlobalAdmin};
use crate::scm::{self, PlainScmCred, ProbeError, ScmBins};
use crate::store::now_ms;
use crate::store::projects::ScmType;

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// 创建期探测请求（测试连接，ad-hoc 凭据不落库）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ScmProbeRequest {
    /// 仓库类型（git / svn）。
    pub scm_type: super::projects::ScmTypeDto,
    /// 仓库 URL。
    pub scm_url: String,
    /// 可选用户名（与 password 一同递送，不落库）。
    #[serde(default)]
    pub username: Option<String>,
    /// 可选密码/token（与 username 一同递送，不落库；永不上命令行/URL）。
    #[serde(default)]
    pub password: Option<String>,
}

/// 测试连接响应：当前 head（git sha / svn revision）；空仓库为 null。
#[derive(Debug, Serialize, ToSchema)]
pub struct ScmProbeResponse {
    /// 当前 head（git commit sha / svn revision）；空仓库为 null。
    pub head: Option<String>,
}

/// 分支枚举请求（git only；创建期默认分支预填）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ScmBranchesRequest {
    /// git 仓库 URL。
    pub scm_url: String,
    /// 可选用户名。
    #[serde(default)]
    pub username: Option<String>,
    /// 可选密码/token。
    #[serde(default)]
    pub password: Option<String>,
}

/// 一个分支（名 + head sha）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ScmBranch {
    /// 分支名。
    pub name: String,
    /// 分支 head commit sha。
    pub head: String,
}

/// 分支枚举响应：分支清单 + 默认分支（远端 HEAD 指向的分支）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ScmBranchesResponse {
    /// 全部分支（`ls-remote --heads`）。
    pub branches: Vec<ScmBranch>,
    /// 默认分支（`--symref HEAD` 解析；detached HEAD 为 null）。
    pub default_branch: Option<String>,
}

/// SCM 凭据设置请求（PUT 整份替换语义；username + password 皆空 = 清凭据）。
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ScmCredentialRequest {
    /// 用户名（非机密；空 = 不设用户名）。
    #[serde(default)]
    pub username: Option<String>,
    /// 密码/token（机密；空 = 不设密码。username + password 皆空 = 清凭据）。
    #[serde(default)]
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------
// 端点
// ---------------------------------------------------------------------------

/// 测试连接（创建期，全局 admin，ad-hoc 凭据不落库）：ls-remote/info 探测
/// 返回当前 head；失败可读错误（凭据错误不回显凭据），不阻塞保存。
#[utoipa::path(
    post,
    path = "/api/v1/projects/scm-probe",
    tag = "scm",
    request_body = ScmProbeRequest,
    responses(
        (status = 200, description = "探测成功，返回当前 head", body = ScmProbeResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 422, description = "探测失败（URL/凭据问题，可读错误；凭据不回显）或 URL 空", body = ErrorBody),
        (status = 500, description = "缺 git/svn 二进制（server 前置未满足，清晰报错）", body = ErrorBody),
    )
)]
pub async fn scm_probe(
    State(_state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
    body: Bytes,
) -> Result<Json<ScmProbeResponse>, ApiError> {
    let req: ScmProbeRequest = parse_body(&body)?;
    validate_url(&req.scm_url)?;
    let cred = build_cred(req.username.as_deref(), req.password.as_deref());
    let bins = ScmBins::default();
    let head = probe_head_for(req.scm_type.into(), &req.scm_url, cred.as_ref(), &bins).await?;
    Ok(Json(ScmProbeResponse { head }))
}

/// 分支枚举（创建期，全局 admin，git only）：ls-remote --heads 列分支 +
/// --symref HEAD 解析默认分支，供新建项目默认分支预填。
#[utoipa::path(
    post,
    path = "/api/v1/projects/scm-branches",
    tag = "scm",
    request_body = ScmBranchesRequest,
    responses(
        (status = 200, description = "分支清单 + 默认分支", body = ScmBranchesResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 422, description = "探测失败（URL/凭据问题）或 URL 空", body = ErrorBody),
        (status = 500, description = "缺 git 二进制", body = ErrorBody),
    )
)]
pub async fn scm_branches(
    State(_state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
    body: Bytes,
) -> Result<Json<ScmBranchesResponse>, ApiError> {
    let req: ScmBranchesRequest = parse_body(&body)?;
    validate_url(&req.scm_url)?;
    let cred = build_cred(req.username.as_deref(), req.password.as_deref());
    let bins = ScmBins::default();
    // 单次 `git ls-remote --symref --heads`：分支列表 + 默认分支同取，免对
    // 私有仓库发两次凭据递送（ADR-0016）。
    let (heads, default_branch) = scm::git_ls_remote_branches(&req.scm_url, cred.as_ref(), &bins)
        .await
        .map_err(probe_err_to_api)?;
    let branches = heads
        .into_iter()
        .map(|(name, head)| ScmBranch { name, head })
        .collect();
    Ok(Json(ScmBranchesResponse {
        branches,
        default_branch,
    }))
}

/// 测试连接（既有项目，项目 admin，存储凭据）：解密项目 SCM 凭据探测 head
/// （项目设置「测试连接」，ADR-0016）。
#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/test-connection",
    tag = "scm",
    params(("name" = String, Path, description = "项目名")),
    responses(
        (status = 200, description = "探测成功，返回当前 head", body = ScmProbeResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见", body = ErrorBody),
        (status = 422, description = "探测失败（URL/凭据问题；凭据不回显）", body = ErrorBody),
        (status = 500, description = "缺 git/svn 二进制或凭据解密失败", body = ErrorBody),
    )
)]
pub async fn test_connection(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
) -> Result<Json<ScmProbeResponse>, ApiError> {
    let row = state.scm_credentials.get(access.project.id).await?;
    let cred = scm::resolve_plain_cred(row, &state.master_key)
        .map_err(|e| ApiError::internal("scm credential decrypt", &e))?;
    let head = probe_head_for(
        access.project.scm_type,
        &access.project.scm_url,
        cred.as_ref(),
        &ScmBins::default(),
    )
    .await?;
    Ok(Json(ScmProbeResponse { head }))
}

/// 设置/清空 SCM 凭据（项目 admin）：username + password 加密落库（PUT 整份
/// 替换；皆空 = 清），审计 `scm_credential_set`。密码经 XChaCha20-Poly1305
/// 加密（复用机密同套，ADR-0015），永不可读回。
#[utoipa::path(
    put,
    path = "/api/v1/projects/{name}/scm-credential",
    tag = "scm",
    request_body = ScmCredentialRequest,
    params(("name" = String, Path, description = "项目名")),
    responses(
        (status = 204, description = "已写入/清空：加密落库，任何端点不回显值"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见", body = ErrorBody),
    )
)]
pub async fn put_scm_credential(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let req: ScmCredentialRequest = parse_body(&body)?;
    let username = req.username.filter(|s| !s.is_empty());
    let ciphertext = match req.password.as_deref() {
        Some(p) if !p.is_empty() => Some(
            crate::secrets::encrypt(&state.master_key, p.as_bytes())
                .map_err(|e| ApiError::internal("scm credential encrypt", &e))?,
        ),
        _ => None,
    };
    let clearing = username.is_none() && ciphertext.is_none();
    state
        .scm_credentials
        .set(
            access.project.id,
            username.as_deref(),
            ciphertext.as_deref(),
            &access.operator,
            now_ms(),
        )
        .await?;
    // 审计（ADR-0015）：SCM 凭据 set/clear——detail 只记动作，永不记值。
    state
        .audit
        .insert(
            now_ms(),
            &access.operator,
            crate::store::audit::AuditEvent::ScmCredentialSet,
            Some(&access.project.name),
            Some(
                &serde_json::json!({ "action": if clearing { "clear" } else { "set" } })
                    .to_string(),
            ),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// 组装辅助
// ---------------------------------------------------------------------------

/// 仓库 URL 非空校验（422）。
fn validate_url(url: &str) -> Result<(), ApiError> {
    if url.trim().is_empty() {
        return Err(ApiError::validation(
            "SCM 探测输入校验失败",
            vec![ValidationIssue {
                path: "scm_url".into(),
                message: "仓库 URL 不能为空".into(),
            }],
        ));
    }
    Ok(())
}

/// 由可选用户名 + 密码构造探测凭据（皆空 → None：免认证仓库）。
fn build_cred(username: Option<&str>, password: Option<&str>) -> Option<PlainScmCred> {
    let u = username.unwrap_or_default();
    let p = password.unwrap_or_default();
    if u.is_empty() && p.is_empty() {
        None
    } else {
        Some(PlainScmCred::new(u.to_string(), p.to_string()))
    }
}

/// 按 scm_type 探测 head（git ls-remote HEAD / svn info revision）。
async fn probe_head_for(
    scm_type: ScmType,
    url: &str,
    cred: Option<&PlainScmCred>,
    bins: &ScmBins,
) -> Result<Option<String>, ApiError> {
    match scm_type {
        ScmType::Git => scm::git_ls_remote_head(url, cred, bins)
            .await
            .map(|(sha, _)| sha)
            .map_err(probe_err_to_api),
        ScmType::Svn => scm::svn_info_revision(url, cred, bins)
            .await
            .map_err(probe_err_to_api),
    }
}

/// 探测错误 → API 错误。缺二进制 = 500（server 前置未满足，清晰报错不静默，
/// ADR-0016）；其余 = 422（URL/凭据问题，可读错误；凭据不回显）。
fn probe_err_to_api(e: ProbeError) -> ApiError {
    let msg = e.message();
    match e {
        ProbeError::MissingBinary(_) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "SCM_BINARY_MISSING",
            msg,
            None,
        ),
        ProbeError::AuthFailed | ProbeError::RepoNotFound | ProbeError::Other { .. } => {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "SCM_PROBE_FAILED",
                msg,
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cred_none_when_both_empty() {
        assert!(build_cred(None, None).is_none());
        assert!(build_cred(Some(""), Some("")).is_none(), "空串视为无凭据");
        assert!(build_cred(Some(""), None).is_none());
    }

    #[test]
    fn build_cred_some_when_either_nonempty() {
        let c = build_cred(Some("alice"), Some("pw")).unwrap();
        assert_eq!(c.username, "alice");
        assert_eq!(c.password, "pw");
        // 仅用户名。
        let c = build_cred(Some("alice"), None).unwrap();
        assert_eq!(c.username, "alice");
        assert_eq!(c.password, "");
    }

    #[test]
    fn probe_err_to_api_missing_binary_is_500_others_422() {
        assert_eq!(
            probe_err_to_api(ProbeError::MissingBinary("git")).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            probe_err_to_api(ProbeError::AuthFailed).status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            probe_err_to_api(ProbeError::RepoNotFound).status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            probe_err_to_api(ProbeError::Other { hint: "x".into() }).status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
