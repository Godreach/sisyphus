//! 项目机密端点（票 B2b-T6，ADR-0015）：建/覆写 / 列名 / 删。
//!
//! 值只写不读：本模块只有 PUT（建/覆写）、GET（仅名清单）、DELETE 三个
//! 端点——任何路径都不回显值，v1 REST 面永无读值端点。项目 admin 档
//! （[`super::policy::RequireAdmin`] 声明）：viewer / runner 访问一律 403
//! （连名都不列——403 来自授权 extractor，先于本模块任何查询）。
//!
//! 机密名进校验（非空、env 键合法字符集：ASCII 字母数字 + `_`，与
//! sisyphus-model 变量名同字符集——机密经 env 注入，键名语义一致）。
//! 非法名 422。值加密（XChaCha20-Poly1305，[`crate::secrets`]）在本模块
//! 写入路径调用一次：密文落库，明文不在任何模块留存。
//!
//! PUT 覆写语义：同名即覆写（唯一键 (project_id, name) 以 ON CONFLICT
//! DO UPDATE 呈现），成功 204 与新建同形（响应无值——覆写与否由列表
//! 长度变化可见，值不回显）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireAdmin;
use crate::store::now_ms;

/// 机密名请求体：值只写不读——value 出现在请求里，此后任何响应不再出现。
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutSecretRequest {
    /// 机密值（写入即加密落库；永不可读回，请谨慎覆写）。
    pub value: String,
}

/// GET 清单条目：仅名（值形态不存在——机密值任何端点不回显）。
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretNameResponse {
    /// 机密名。
    pub name: String,
}

/// 机密名是否合法：非空、ASCII 字母数字或 `_`（env 键合法字符集）。
///
/// 委托 sisyphus-model 的变量名校验（单一事实源——机密经 env 注入，键名
/// 语义与变量名同字符集；B2b 不立第二份规则）。
pub fn is_valid_secret_name(name: &str) -> bool {
    sisyphus_model::variables::is_valid_name(name)
}

/// 建/覆写机密（项目 admin 档；值只写不读，成功响应无值形态）。
#[utoipa::path(
    put,
    path = "/api/v1/projects/{name}/secrets/{secret}",
    tag = "secrets",
    request_body = PutSecretRequest,
    params(
        ("name" = String, Path, description = "项目名"),
        ("secret" = String, Path, description = "机密名（env 键字符集：字母数字与下划线）"),
    ),
    responses(
        (status = 204, description = "已写入：值加密落库，任何端点不再回显"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（机密管理需项目 admin 档；viewer/runner 连名不可见）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见", body = ErrorBody),
        (status = 422, description = "输入校验失败（机密名非法）", body = ErrorBody),
    )
)]
pub async fn put_secret(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project_name, secret)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    validate_secret_name(&secret)?;
    let req: PutSecretRequest = parse_body(&body)?;

    // 加密只在写入路径调用一次：明文经此即不留存于任何模块。
    let blob = crate::secrets::encrypt(&state.master_key, req.value.as_bytes()).map_err(
        |e| ApiError::internal("secret encrypt", &e),
    )?;
    state
        .secrets
        .upsert(access.project.id, &secret, &blob, &access.operator, now_ms())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 列项目机密名（项目 admin 档：viewer/runner 连名都 403，由 extractor
/// 裁决、本端点不可达）。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/secrets",
    tag = "secrets",
    params(("name" = String, Path, description = "项目名")),
    responses(
        (status = 200, description = "机密名清单（按名排序；值永不可读，任何端点不回显）", body = [SecretNameResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（机密管理需项目 admin 档；viewer/runner 连名不可见）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见", body = ErrorBody),
    )
)]
pub async fn list_secrets(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
) -> Result<Json<Vec<SecretNameResponse>>, ApiError> {
    let names = state.secrets.list_names(access.project.id).await?;
    Ok(Json(
        names
            .into_iter()
            .map(|name| SecretNameResponse { name })
            .collect(),
    ))
}

/// 删除机密（项目 admin 档：名消失即 DELETE 后的可观察语义）。
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{name}/secrets/{secret}",
    tag = "secrets",
    params(
        ("name" = String, Path, description = "项目名"),
        ("secret" = String, Path, description = "机密名"),
    ),
    responses(
        (status = 204, description = "已删除：名从清单消失"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（机密管理需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见，或机密名不存在", body = ErrorBody),
    )
)]
pub async fn delete_secret(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project_name, secret)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    validate_secret_name(&secret)?;
    if state.secrets.delete(access.project.id, &secret).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::resource_not_found(format!("机密不存在：{secret}")))
    }
}

/// 机密名校验：非空、env 键合法字符集（字母数字 + `_`）。非法即 422，
/// 不落任何写入。DELETE 同走此校验（端点族规则统一——非法名按定义不可
/// 能存在，422 先于 404 给出「名非法」而非「不存在」的语义）。
fn validate_secret_name(name: &str) -> Result<(), ApiError> {
    if is_valid_secret_name(name) {
        Ok(())
    } else {
        Err(ApiError::validation(
            "机密名校验失败",
            vec![ValidationIssue {
                path: "secret".into(),
                message: "机密名须为非空、由字母数字与下划线组成（env 键字符集）".into(),
            }],
        ))
    }
}
