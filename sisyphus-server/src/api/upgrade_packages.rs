//! 升级包管理端点（票 #76 / B5-T4，ADR-0017）：
//! - **上传**（`POST /upgrade-packages`，全局 admin）：raw octet-stream body +
//!   `X-Sisyphus-Filename` 头携带包名；按 ADR-0010 文件名规范解析版本与目标
//!   三元组、窗口校验（≥ N-1 且 ≤ Server 版本，窗外 409）、落盘 + 记 sha256。
//! - **列表**（`GET /upgrade-packages`，全局 admin）：已上传包清单。
//! - **删除**（`DELETE /upgrade-packages/{package_name}`，全局 admin）：删旧包
//!   （元数据 + 字节）。
//! - **下载**（`GET /agent/upgrade-packages/{package_name}`，**Agent token 鉴权**）：
//!   Agent 凭 `sisa_` token 拉取包字节（与产物上传同模式，ADR-0007：大文件走
//!   HTTP 不走 gRPC 流）；响应头带 size/sha256，Agent 侧下载后校验。
//!
//! 「一次多包」由前端连续多次 POST 实现（与产物单文件 raw octet 上传一致，
//! 不引 multipart 依赖）。包名直接成为磁盘路径段与 URL 段，非法名在 store
//! 层 [`validate_package_name`] 与此处双拒。

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::artifacts::AgentAuth;
use super::error::{ApiError, ErrorBody, ValidationIssue};
use super::policy::RequireGlobalAdmin;
use super::agents::VersionDto;
use crate::store::agents::AgentVersion;
use crate::store::audit::AuditEvent;
use crate::store::now_ms;
use crate::store::upgrade_packages::validate_package_name;

/// 升级包视图（与 [`crate::store::upgrade_packages::UpgradePackageMeta`] 同构）。
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct UpgradePackageResponse {
    /// 包名（ADR-0010 规范 `sisyphus-agent-<ver>-<os>-<arch>`）。
    pub package_name: String,
    /// 解析自文件名的版本。
    pub version: VersionDto,
    /// 目标 OS（linux/macos/windows）。
    pub target_os: String,
    /// 目标架构（x86_64/aarch64）。
    pub target_arch: String,
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
    /// 上传时刻（Unix 毫秒）。
    pub created_at: i64,
}

/// 上传升级包（全局 admin）：raw octet-stream body + `X-Sisyphus-Filename` 头。
#[utoipa::path(
    post,
    path = "/api/v1/upgrade-packages",
    tag = "upgrade-packages",
    responses(
        (status = 201, description = "已上传；返回包元数据（含解析版本/目标/sha256）", body = UpgradePackageResponse),
        (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody),
        (status = 409, description = "版本窗外（< N-1 或 > Server）", body = ErrorBody),
        (status = 422, description = "文件名缺/不可解析/目标三元组非法", body = ErrorBody),
    )
)]
pub async fn upload(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<UpgradePackageResponse>), ApiError> {
    let package_name = filename_from_headers(&headers)?;
    // 包名安全（路径段/控制字符）+ 文件名规范解析（版本 + 目标三元组）。
    validate_package_name(&package_name).map_err(name_invalid)?;
    let (version, target_os, target_arch) = parse_package_filename(&package_name)?;
    // 窗口校验（ADR-0017：≥ N-1 且 ≤ Server 版本；窗外拒收）。
    let server_version = state.agents.server_version();
    if version.too_old(&server_version) {
        return Err(ApiError::conflict(format!(
            "升级包版本 {}.{}.{} 过旧（低于 N-1 兼容窗口下界，Server 为 {}.{}.{}）",
            version.major, version.minor, version.patch,
            server_version.major, server_version.minor, server_version.patch
        )));
    }
    if version.too_new(&server_version) {
        return Err(ApiError::conflict(format!(
            "升级包版本 {}.{}.{} 过新（高于 Server {}.{}.{}，Server 须先升）",
            version.major, version.minor, version.patch,
            server_version.major, server_version.minor, server_version.patch
        )));
    }

    // 流式落盘（.part + 原子 rename + 边写边算 sha256）——与产物上传同模式。
    let stream = body
        .into_data_stream()
        .map(|r| r.map(|b| b.to_vec()).map_err(std::io::Error::other))
        .boxed();
    let bytes = state
        .upgrade_packages
        .store(&package_name, stream)
        .await
        .map_err(|e| ApiError::internal("升级包落盘", &e))?;
    let meta = crate::store::upgrade_packages::UpgradePackageMeta {
        package_name: package_name.clone(),
        version,
        target_os,
        target_arch,
        size: bytes.size,
        sha256: bytes.sha256,
        created_at: now_ms(),
    };
    state
        .upgrade_package_meta
        .record(&meta)
        .await
        .map_err(|e| ApiError::internal("升级包元数据落库", &e))?;

    state
        .audit
        .insert(
            now_ms(),
            &auth.username,
            AuditEvent::UpgradePackageUploaded,
            None,
            Some(
                &serde_json::json!({
                    "package": meta.package_name,
                    "version": format!("{}.{}.{}", meta.version.major, meta.version.minor, meta.version.patch),
                    "target": format!("{}/{}", meta.target_os, meta.target_arch),
                })
                .to_string(),
            ),
        )
        .await?;

    Ok((StatusCode::CREATED, Json(to_response(meta))))
}

/// 升级包清单（全局 admin；按包名排序）。
#[utoipa::path(
    get,
    path = "/api/v1/upgrade-packages",
    tag = "upgrade-packages",
    responses(
        (status = 200, description = "全部升级包（按包名排序）", body = [UpgradePackageResponse]),
        (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<Vec<UpgradePackageResponse>>, ApiError> {
    let rows = state.upgrade_package_meta.list().await?;
    Ok(Json(rows.into_iter().map(to_response).collect()))
}

/// 删除升级包（全局 admin）：删元数据行 + 字节文件。
#[utoipa::path(
    delete,
    path = "/api/v1/upgrade-packages/{package_name}",
    tag = "upgrade-packages",
    params(("package_name" = String, Path, description = "包名")),
    responses(
        (status = 204, description = "已删除"),
        (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody),
        (status = 404, description = "包不存在", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    Path(package_name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let deleted = state.upgrade_package_meta.delete(&package_name).await?;
    if !deleted {
        return Err(ApiError::resource_not_found(format!(
            "升级包 {package_name} 不存在"
        )));
    }
    // 字节文件幂等删除（元数据已删即视为成功，残留字节无害）。
    let _ = state.upgrade_packages.delete(&package_name).await;
    state
        .audit
        .insert(
            now_ms(),
            &auth.username,
            AuditEvent::UpgradePackageDeleted,
            None,
            Some(&serde_json::json!({ "package": package_name }).to_string()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 升级包的 Agent 下载相对路径（`UpgradeCommand.download_url` 填此；Agent 侧
/// 按 `api_url` 解析为绝对——与产物上传同模式，ADR-0007/0017）。路由定义在本
/// 模块（[`download`]），升级指令下发端点（`api/agents`）经此函数构造
/// `download_url`，避免两处字符串各写一份漂移。
pub fn agent_download_path(package_name: &str) -> String {
    format!("/api/v1/agent/upgrade-packages/{package_name}")
}

/// 下载升级包（Agent token 鉴权；Agent 侧拉取后校验 sha256、原子换入）。
#[utoipa::path(
    get,
    path = "/api/v1/agent/upgrade-packages/{package_name}",
    tag = "upgrade-packages",
    params(("package_name" = String, Path, description = "包名")),
    responses(
        (status = 200, description = "包字节流（响应头带 size/sha256）"),
        (status = 401, description = "Agent token 无效/已停用", body = ErrorBody),
        (status = 404, description = "包不存在", body = ErrorBody),
    )
)]
pub async fn download(
    State(state): State<AppState>,
    Extension(_agent): Extension<AgentAuth>,
    Path(package_name): Path<String>,
) -> Result<Response, ApiError> {
    let meta = state
        .upgrade_package_meta
        .find(&package_name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("升级包 {package_name} 不存在")))?;
    let stream = state
        .upgrade_packages
        .open(&package_name)
        .await
        .map_err(|e| ApiError::internal("升级包读取", &e))?;
    let body = Body::from_stream(stream.map(|r| r.map(axum::body::Bytes::from)));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_LENGTH,
        meta.size.to_string().parse().expect("长度为合法头值"),
    );
    headers.insert(
        header::HeaderName::from_static("x-sisyphus-sha256"),
        meta.sha256.parse().expect("sha256 hex 为合法头值"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{package_name}\"")
            .parse()
            .expect("包名为合法头值"),
    );
    Ok((StatusCode::OK, headers, body).into_response())
}

// ---------------------------------------------------------------------------
// 组装辅助
// ---------------------------------------------------------------------------

/// `UpgradePackageMeta` → 响应 DTO。
fn to_response(meta: crate::store::upgrade_packages::UpgradePackageMeta) -> UpgradePackageResponse {
    UpgradePackageResponse {
        package_name: meta.package_name,
        version: VersionDto {
            major: meta.version.major,
            minor: meta.version.minor,
            patch: meta.version.patch,
        },
        target_os: meta.target_os,
        target_arch: meta.target_arch,
        size: meta.size,
        sha256: meta.sha256,
        created_at: meta.created_at,
    }
}

/// 文件名头名（raw octet 上传时包名经此头携带）。
const FILENAME_HEADER: &str = "x-sisyphus-filename";

/// 从 `X-Sisyphus-Filename` 头取包名（缺头/空 422）。
fn filename_from_headers(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(FILENAME_HEADER)
        .ok_or_else(|| {
            ApiError::validation(
                "升级包上传校验失败",
                vec![ValidationIssue {
                    path: "X-Sisyphus-Filename".into(),
                    message: "缺 X-Sisyphus-Filename 头（包名）".into(),
                }],
            )
        })?;
    let name = value
        .to_str()
        .map_err(|_| {
            ApiError::validation(
                "升级包上传校验失败",
                vec![ValidationIssue {
                    path: "X-Sisyphus-Filename".into(),
                    message: "X-Sisyphus-Filename 头不是合法文本".into(),
                }],
            )
        })?
        .trim();
    if name.is_empty() {
        return Err(ApiError::validation(
            "升级包上传校验失败",
            vec![ValidationIssue {
                path: "X-Sisyphus-Filename".into(),
                message: "包名不能为空".into(),
            }],
        ));
    }
    Ok(name.to_string())
}

/// 按 ADR-0010 文件名规范解析 `sisyphus-agent-<ver>-<os>-<arch>[.tar.gz|.zip|.tar]`
/// → (版本, 目标 OS, 目标架构)。不可解析 → 422。
pub(crate) fn parse_package_filename(name: &str) -> Result<(AgentVersion, String, String), ApiError> {
    const PREFIX: &str = "sisyphus-agent-";
    let stem = name
        .strip_prefix(PREFIX)
        .ok_or_else(|| filename_issue("包名须以 sisyphus-agent- 开头"))?;
    // 去扩展名（.tar.gz / .zip / .tar；顺序敏感：先 .tar.gz）。
    let stem = stem
        .strip_suffix(".tar.gz")
        .or_else(|| stem.strip_suffix(".tar"))
        .unwrap_or_else(|| stem.strip_suffix(".zip").unwrap_or(stem));
    let mut parts = stem.split('-');
    let version_str = parts
        .next()
        .ok_or_else(|| filename_issue("包名缺版本段"))?;
    let target_os = parts
        .next()
        .ok_or_else(|| filename_issue("包名缺目标 OS 段"))?
        .to_string();
    let target_arch = parts
        .next()
        .ok_or_else(|| filename_issue("包名缺目标架构段"))?
        .to_string();
    if parts.next().is_some() {
        return Err(filename_issue("包名段数过多（期望 version-os-arch）"));
    }
    // 版本 "major.minor.patch"。
    let mut nums = version_str.split('.');
    let major = nums
        .next()
        .ok_or_else(|| filename_issue("版本段缺 major"))?
        .parse::<u32>()
        .map_err(|_| filename_issue("版本 major 须为数字"))?;
    let minor = nums
        .next()
        .ok_or_else(|| filename_issue("版本段缺 minor"))?
        .parse::<u32>()
        .map_err(|_| filename_issue("版本 minor 须为数字"))?;
    let patch = nums
        .next()
        .ok_or_else(|| filename_issue("版本段缺 patch"))?
        .parse::<u32>()
        .map_err(|_| filename_issue("版本 patch 须为数字"))?;
    if nums.next().is_some() {
        return Err(filename_issue("版本段数过多（期望 major.minor.patch）"));
    }
    // 目标三元组取值域（ADR-0010 发布矩阵）。
    if !matches!(target_os.as_str(), "linux" | "macos" | "windows") {
        return Err(filename_issue("目标 OS 须为 linux/macos/windows"));
    }
    if !matches!(target_arch.as_str(), "x86_64" | "aarch64") {
        return Err(filename_issue("目标架构须为 x86_64/aarch64"));
    }
    Ok((
        AgentVersion {
            major,
            minor,
            patch,
        },
        target_os,
        target_arch,
    ))
}

/// 包名/文件名问题 → 422 校验错误。
fn filename_issue(message: &str) -> ApiError {
    ApiError::validation(
        "升级包文件名不可解析",
        vec![ValidationIssue {
            path: "X-Sisyphus-Filename".into(),
            message: message.into(),
        }],
    )
}

/// store 包名校验失败 → 422。
fn name_invalid(e: crate::store::StoreError) -> ApiError {
    ApiError::validation(
        "升级包名非法",
        vec![ValidationIssue {
            path: "X-Sisyphus-Filename".into(),
            message: e.to_string(),
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tar_gz_zip_and_tar_extensions() {
        let (v, os, arch) = parse_package_filename("sisyphus-agent-1.0.0-linux-x86_64.tar.gz")
            .expect("tar.gz");
        assert_eq!(v, AgentVersion { major: 1, minor: 0, patch: 0 });
        assert_eq!(os, "linux");
        assert_eq!(arch, "x86_64");

        let (v, os, arch) = parse_package_filename("sisyphus-agent-0.9.5-macos-aarch64.zip")
            .expect("zip");
        assert_eq!(v, AgentVersion { major: 0, minor: 9, patch: 5 });
        assert_eq!(os, "macos");
        assert_eq!(arch, "aarch64");

        let (v, _, _) = parse_package_filename("sisyphus-agent-1.2.3-windows-x86_64.tar")
            .expect("tar");
        assert_eq!(v, AgentVersion { major: 1, minor: 2, patch: 3 });
    }

    #[test]
    fn rejects_bad_prefix_missing_segments_and_bad_targets() {
        // 前缀错。
        assert!(parse_package_filename("agent-1.0.0-linux-x86_64.tar.gz").is_err());
        // 段数不足。
        assert!(parse_package_filename("sisyphus-agent-1.0.0-linux.tar.gz").is_err());
        assert!(parse_package_filename("sisyphus-agent-1.0.0.tar.gz").is_err());
        // 段数过多。
        assert!(parse_package_filename("sisyphus-agent-1.0.0-linux-x86_64-extra.tar.gz").is_err());
        // 版本非数字 / 段数错。
        assert!(parse_package_filename("sisyphus-agent-1.0-linux-x86_64.tar.gz").is_err());
        assert!(parse_package_filename("sisyphus-agent-x.y.z-linux-x86_64.tar.gz").is_err());
        assert!(parse_package_filename("sisyphus-agent-1.0.0.1-linux-x86_64.tar.gz").is_err());
        // 目标 OS/架构非法。
        assert!(parse_package_filename("sisyphus-agent-1.0.0-freebsd-x86_64.tar.gz").is_err());
        assert!(parse_package_filename("sisyphus-agent-1.0.0-linux-armv7.tar.gz").is_err());
    }

    #[test]
    fn filename_from_headers_reads_x_sisyphus_filename() {
        let mut h = HeaderMap::new();
        assert!(filename_from_headers(&h).is_err(), "缺头应拒");
        h.insert(
            FILENAME_HEADER,
            "sisyphus-agent-1.0.0-linux-x86_64.tar.gz"
                .parse()
                .expect("值"),
        );
        assert_eq!(
            filename_from_headers(&h).unwrap(),
            "sisyphus-agent-1.0.0-linux-x86_64.tar.gz"
        );
        h.insert(FILENAME_HEADER, "  ".parse().expect("空值"));
        assert!(filename_from_headers(&h).is_err(), "空白应拒");
    }
}
