//! 可选 S3 存储契约（票 #122，ADR-0026）：配置身份、对象键、启动校验与连接自检。

pub mod keys;
pub mod s3;

use sqlx::SqlitePool;

use crate::config::S3Config;
use crate::store::{S3BackendIdentity, S3IdentityRepo, StoreError};

pub use keys::{ObjectClass, ObjectPhase, object_key, probe_key};
pub use s3::{S3Client, S3PublicView};

/// 存储面错误（启动校验 / 连接自检）。错误正文不含凭据。
#[derive(Debug)]
pub enum StorageError {
    /// 配置或 TLS 材料非法。
    Config(String),
    /// 凭据被拒绝。
    Credentials(String),
    /// bucket 不存在或不可见。
    MissingBucket(String),
    /// S3 协议/响应不符合契约。
    Protocol(String),
    /// 远端 S3 其它错误。
    S3 {
        /// HTTP 状态。
        status: u16,
        /// S3 错误码（可空）。
        code: String,
        /// 不含凭据的说明。
        message: String,
    },
    /// 身份表。
    Store(StoreError),
    /// HTTP 传输。
    Transport(String),
}

impl StorageError {
    pub(crate) fn from_reqwest(err: reqwest::Error) -> Self {
        // URL 可能含 bucket/key，但不含签名查询（本客户端走头签名）。
        Self::Transport(format!("S3 请求失败：{err}"))
    }
}

impl From<StoreError> for StorageError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m)
            | Self::Credentials(m)
            | Self::MissingBucket(m)
            | Self::Protocol(m)
            | Self::Transport(m) => {
                write!(f, "{m}")
            }
            Self::S3 {
                status,
                code,
                message,
            } => {
                if code.is_empty() {
                    write!(f, "S3 错误 {status}：{message}")
                } else {
                    write!(f, "S3 错误 {status}/{code}：{message}")
                }
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// 单步连接自检结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionCheck {
    /// 操作名（put/head/range_get/copy/multipart_upload/multipart_copy/delete）。
    pub op: String,
    /// 是否成功。
    pub ok: bool,
    /// 失败说明（不含凭据）。
    pub detail: Option<String>,
}

/// 管理员显式测试报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionTestReport {
    /// 全部步骤成功。
    pub ok: bool,
    /// 逐步结果。
    pub checks: Vec<ConnectionCheck>,
}

/// 启动时装配可选 S3：未配置返回 None；已配置则非破坏性校验 bucket 并核对身份。
pub async fn prepare_s3(
    pool: &SqlitePool,
    cfg: Option<&S3Config>,
) -> Result<Option<S3Client>, StorageError> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };
    let client = S3Client::new(cfg)?;
    client.head_bucket().await?;
    S3IdentityRepo::new(pool.clone())
        .ensure_compatible(&S3BackendIdentity::from_config(cfg))
        .await?;
    Ok(Some(client))
}
