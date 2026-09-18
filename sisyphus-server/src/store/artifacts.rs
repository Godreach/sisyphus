//! 产物存储（票 #74/#121，ADR-0004/0026）：本地磁盘字节 + 可区分后端与
//! 任务 attempt 归属的 SQLite 元数据两层。
//!
//! - **字节层**（[`LocalDiskArtifactStore`]）：布局 `data/artifacts/<build_id>/
//!   <name>`（ADR-0004）。写入流式落 `.part` 临时文件、边写边算 SHA-256 与
//!   字节数，成功后原子 rename 到最终名（半截文件不可见）；读取按 64 KiB
//!   块流式回放（HTTP 下载响应体）。产物名即磁盘路径段：含路径分隔符或
//!   `..` 的名在 [`validate_artifact_name`] 拒绝（API 层同规则 422，此处
//!   防御性兜底）。
//! - **元数据层**（[`SqliteArtifactMetaRepo`]）：`artifacts` 表记录后端、
//!   build/job/attempt 归属、正文状态、路径、大小和校验和。历史行迁移为
//!   `local/ready` 且保留原 30 天清理；旧 schema 无法恢复的 job/attempt
//!   保持空。带任务归属的行按 `(build, job, attempt, name)` 隔离；无归属的
//!   legacy 行继续保持 `(build, name)` 覆盖兼容。
//!
//! Agent 上传端点（`api::artifacts`，agent token 鉴权）消费两层：字节流经
//! [`ArtifactStore::store`] 落盘、返回的元数据行经 [`ArtifactMetaRepo::record`]
//! 落库；下载端点（Agent 依赖拉取 + 构建详情页）经 [`ArtifactMetaRepo::find`]
//! 取大小/校验和做响应头、[`ArtifactStore::open`] 取字节流。

use std::path::{Path, PathBuf};

use futures::StreamExt;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::StoreError;
use super::traits::{
    ArtifactBackend, ArtifactMeta, ArtifactMetaRepo, ArtifactState, ArtifactStore, ByteStream,
};

/// 产物名长度上限（磁盘路径段 + URL 路径段的宽松界）。
pub const ARTIFACT_NAME_MAX: usize = 128;

/// 流式读写块大小（64 KiB：与日志 chunk 同量级，大文件往返次数与内存
/// 占用的折中）。
const IO_CHUNK: usize = 64 * 1024;

/// 产物名校验：非空、无路径分隔符（`/` `\`）、非 `.`/`..`、无控制字符、
/// 长度 <= [`ARTIFACT_NAME_MAX`]——产物名直接成为磁盘路径段与 URL 段，
/// 非法名在这里与 API 层（422）双重拒绝。
pub fn validate_artifact_name(name: &str) -> Result<(), StoreError> {
    let invalid = |what: &str| StoreError::Invalid(format!("产物名非法（{what}）：{name}"));
    if name.trim().is_empty() {
        return Err(invalid("空名"));
    }
    if name.len() > ARTIFACT_NAME_MAX {
        return Err(invalid(&format!("超过 {ARTIFACT_NAME_MAX} 字符")));
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(invalid("不得为路径段"));
    }
    if name.chars().any(char::is_control) {
        return Err(invalid("含控制字符"));
    }
    Ok(())
}

/// 本地磁盘产物字节存储（[`ArtifactStore`] 的生产实现，ADR-0004 布局）。
#[derive(Debug, Clone)]
pub struct LocalDiskArtifactStore {
    /// 产物根（数据目录 `artifacts/`，config 建好布局）。
    root: PathBuf,
}

impl LocalDiskArtifactStore {
    /// 以产物根构造（目录由 config 布局保证存在；此处不重复建）。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 产物根目录（保留清理 / 手动删构建的字节裁剪面，与上传下载同根）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 按任务 attempt 隔离磁盘正文，同时保留对外产物名。
    pub async fn store_for_job(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        public_name: &str,
        content: ByteStream,
    ) -> Result<ArtifactMeta, StoreError> {
        // 产物名上限 128 字节，内部键不能简单拼接前缀后再过同一上限；
        // 用摘要保留稳定、短且不含路径分隔符的任务隔离键。
        let digest = format!("{:x}", Sha256::digest(public_name.as_bytes()));
        let storage_name = format!(".j{job_id}-{attempt}-{}", &digest[..16]);
        let mut meta = self.store(build_id, &storage_name, content).await?;
        meta.name = public_name.to_string();
        Ok(meta)
    }

    /// 产物的磁盘路径：`<root>/<build_id>/<name>`（调用侧已过名校验）。
    fn artifact_path(&self, build_id: i64, name: &str) -> PathBuf {
        self.root.join(build_id.to_string()).join(name)
    }
}

impl ArtifactStore for LocalDiskArtifactStore {
    async fn store(
        &self,
        build_id: i64,
        name: &str,
        content: ByteStream,
    ) -> Result<ArtifactMeta, StoreError> {
        self.store_checked(build_id, name, content, None).await
    }

    async fn open(&self, build_id: i64, name: &str) -> Result<ByteStream, StoreError> {
        self.open_stream(build_id, name).await
    }

    async fn open_meta(&self, meta: &ArtifactMeta) -> Result<ByteStream, StoreError> {
        let file = tokio::fs::File::open(self.root.join(&meta.path)).await?;
        let stream = futures::stream::unfold(file, |mut file| async move {
            let mut buf = vec![0u8; IO_CHUNK];
            match file.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(buf), file))
                }
                Err(e) => Some((Err(e), file)),
            }
        });
        Ok(stream.boxed())
    }

    async fn inspect_state(&self, meta: &ArtifactMeta) -> Result<ArtifactState, StoreError> {
        if meta.backend != ArtifactBackend::Local {
            return Err(StoreError::Invalid("本地存储不能检查非本地产物".into()));
        }
        match tokio::fs::metadata(self.root.join(&meta.path)).await {
            Ok(m) if m.is_file() && m.len() == meta.size => Ok(ArtifactState::Ready),
            Ok(_) => Ok(ArtifactState::Missing),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ArtifactState::Missing),
            Err(e) => Err(e.into()),
        }
    }
}

impl LocalDiskArtifactStore {
    /// 在正文可见前核验清单摘要与大小，错误内容只留在临时文件并清理。
    pub async fn store_verified(
        &self,
        build_id: i64,
        name: &str,
        content: ByteStream,
        size: u64,
        sha256: &str,
    ) -> Result<ArtifactMeta, StoreError> {
        self.store_checked(build_id, name, content, Some((size, sha256)))
            .await
    }

    async fn store_checked(
        &self,
        build_id: i64,
        name: &str,
        content: ByteStream,
        expected: Option<(u64, &str)>,
    ) -> Result<ArtifactMeta, StoreError> {
        validate_artifact_name(name)?;
        let dir = self.root.join(build_id.to_string());
        tokio::fs::create_dir_all(&dir).await?;

        // 半截写入不可见：先落 .part 临时文件（同目录保证 rename 原子），
        // 流尽且校验和算完才 rename 到最终名。失败清理临时文件。
        let tmp = dir.join(format!(".{name}.part-{}", now_part_suffix()));
        let meta = write_stream(&tmp, build_id, name, content)
            .await
            .and_then(|meta| {
                if expected.is_some_and(|(size, sha)| meta.size != size || meta.sha256 != sha) {
                    Err(StoreError::Invalid("目录文件与清单大小或摘要不符".into()))
                } else {
                    Ok(meta)
                }
            });
        match meta {
            Ok(meta) => {
                tokio::fs::rename(&tmp, self.artifact_path(build_id, name)).await?;
                Ok(meta)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    async fn open_stream(&self, build_id: i64, name: &str) -> Result<ByteStream, StoreError> {
        validate_artifact_name(name)?;
        let path = self.artifact_path(build_id, name);
        if !tokio::fs::try_exists(&path).await? {
            return Err(StoreError::NotFound(format!("产物 {name} 不存在")));
        }
        let file = tokio::fs::File::open(&path).await?;
        // 64 KiB 块流式回放：EOF 关流；读错误透传（HTTP 层截断响应）。
        let stream = futures::stream::unfold(file, |mut file| async move {
            let mut buf = vec![0u8; IO_CHUNK];
            match file.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(buf), file))
                }
                Err(e) => Some((Err(e), file)),
            }
        });
        Ok(stream.boxed())
    }
}

/// 流式写盘 + 边写边算 SHA-256/字节数，返回元数据（path 为正斜杠相对键，
/// ADR-0004：v2 对象存储迁移留缝）。
async fn write_stream(
    tmp: &Path,
    build_id: i64,
    name: &str,
    mut content: ByteStream,
) -> Result<ArtifactMeta, StoreError> {
    let mut file = tokio::fs::File::create(tmp).await?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    while let Some(chunk) = content.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        hasher.update(&chunk);
        size += chunk.len() as u64;
    }
    file.flush().await?;
    Ok(ArtifactMeta {
        build_id,
        job_id: None,
        attempt: None,
        backend: ArtifactBackend::Local,
        state: ArtifactState::Ready,
        name: name.to_string(),
        path: format!("{build_id}/{name}"),
        size,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// 临时文件名后缀：毫秒时间戳 + 进程内计数（同毫秒多次上传不撞名即可，
/// 非安全面）。
fn now_part_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{seq}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

/// SQLite 产物元数据仓储（[`ArtifactMetaRepo`] 的生产实现）。
#[derive(Debug, Clone)]
pub struct SqliteArtifactMetaRepo {
    pool: SqlitePool,
    /// 保留期天数（config `[retention]` 合并后的全局值，默认 30，ADR-0013；
    /// 上传完成记行的 `retention_until = 落库时刻 + 保留期`，每日清理扫描消费）。
    retention_days: i64,
}

/// 列表条目（含上传时刻——[`ArtifactMeta`] 缝不含时间列，API 列表面消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactMetaEntry {
    /// 产物元数据（名/路径/大小/校验和）。
    pub meta: ArtifactMeta,
    /// 上传时刻（Unix 毫秒；重跑同名再传刷新）。
    pub created_at: i64,
}

/// 目录清单条目的支持类型。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    sqlx::Type,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ArtifactEntryKind {
    /// 普通文件。
    File,
    /// 普通目录。
    Directory,
}

/// 完整清单的发布状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize, utoipa::ToSchema)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ArtifactSetState {
    /// 全部正文尚未核验完成。
    Pending,
    /// 全部正文核验完成，可以整体消费。
    Ready,
}

/// 一次任务 attempt 的完整目录产物清单及发布状态。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, utoipa::ToSchema)]
pub struct ArtifactSetRow {
    /// Server 分配的稳定集合 ID。
    pub id: i64,
    /// 所属构建。
    pub build_id: i64,
    /// 上传任务行 ID。
    pub job_id: i64,
    /// 上传 attempt。
    pub attempt: i32,
    /// 任务声明的产物名。
    pub name: String,
    /// 发布状态：pending 或 ready。
    pub state: ArtifactSetState,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
}

/// 清单内的普通文件或目录；内部文件名不作为授权凭据。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, utoipa::ToSchema)]
pub struct ArtifactSetEntry {
    /// 规范化的目录相对路径。
    pub path: String,
    /// file 或 directory。
    pub kind: ArtifactEntryKind,
    /// 字节数；目录为零。
    pub size: i64,
    /// 小写 SHA-256；目录为空。
    pub sha256: String,
    /// 是否保留 Unix 可执行位。
    pub executable: bool,
    /// Server 分配的私有字节对象名；目录为空。
    pub artifact_name: Option<String>,
}

/// 尚未完成的 S3 multipart 上传会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartUploadRow {
    /// 构建 id。
    pub build_id: i64,
    /// 产物名。
    pub name: String,
    /// 上传任务行 id。
    pub job_id: i64,
    /// 任务 attempt。
    pub attempt: i32,
    /// S3 临时对象 key。
    pub object_key: String,
    /// S3 upload id。
    pub upload_id: String,
    /// 预声明字节数。
    pub size: u64,
    /// 分片字节数。
    pub part_size: u64,
    /// S3 完成 multipart 后已形成临时对象，尚未发布 ready。
    pub completed: bool,
    /// URL/会话过期时刻（Unix 毫秒）。
    pub expires_at: i64,
}

/// 数据库行的具名形态；统一查询 `created_at`，避免多套长元组依赖列序。
#[derive(sqlx::FromRow)]
struct ArtifactRow {
    build_id: i64,
    job_id: Option<i64>,
    attempt: Option<i32>,
    backend: String,
    state: String,
    name: String,
    path: String,
    size: i64,
    sha256: String,
    created_at: i64,
}

impl ArtifactRow {
    fn into_meta(self) -> Result<ArtifactMeta, StoreError> {
        Ok(ArtifactMeta {
            build_id: self.build_id,
            job_id: self.job_id,
            attempt: self.attempt,
            backend: ArtifactBackend::try_from(self.backend.as_str())?,
            state: ArtifactState::try_from(self.state.as_str())?,
            name: self.name,
            path: self.path,
            size: self.size as u64,
            sha256: self.sha256,
        })
    }

    fn into_entry(self) -> Result<ArtifactMetaEntry, StoreError> {
        let created_at = self.created_at;
        Ok(ArtifactMetaEntry {
            meta: self.into_meta()?,
            created_at,
        })
    }
}

impl SqliteArtifactMetaRepo {
    /// 按任务 attempt 查询 pending/ready 元数据，避免同名产物跨任务串读。
    pub async fn find_including_pending_for_job(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<Option<ArtifactMeta>, StoreError> {
        let row = sqlx::query_as::<_, ArtifactRow>(
            "SELECT build_id, job_id, attempt, backend, state, name, path, size, sha256,
                    created_at FROM artifacts
             WHERE build_id = ? AND job_id = ? AND attempt = ? AND name = ?",
        )
        .bind(build_id)
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ArtifactRow::into_meta).transpose()
    }

    /// 按构建内来源任务与 attempt 精确定位依赖产物。
    pub async fn find_for_job(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<Option<ArtifactMeta>, StoreError> {
        let row = sqlx::query_as::<_, ArtifactRow>(
            "SELECT build_id, job_id, attempt, backend, state, name, path, size, sha256,
                    created_at FROM artifacts
             WHERE build_id = ? AND job_id = ? AND attempt = ? AND name = ?
               AND state != 'pending'",
        )
        .bind(build_id)
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ArtifactRow::into_meta).transpose()
    }

    /// 创建不可变清单；同一 attempt 重试必须提供完全相同的条目。
    pub async fn create_set(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
        entries: &[ArtifactSetEntry],
    ) -> Result<(ArtifactSetRow, Vec<ArtifactSetEntry>), StoreError> {
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO artifact_sets (build_id, job_id, attempt, name, state, created_at)
                     VALUES (?, ?, ?, ?, 'pending', ?) ON CONFLICT (job_id, attempt, name) DO NOTHING")
            .bind(build_id).bind(job_id).bind(attempt).bind(name)
            .bind(crate::store::now_ms()).execute(&mut *tx).await?.rows_affected() != 0;
        let set = sqlx::query_as::<_, ArtifactSetRow>(
            "SELECT id, build_id, job_id, attempt, name, state, created_at FROM artifact_sets
             WHERE job_id = ? AND attempt = ? AND name = ?",
        )
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        let existing = sqlx::query_as::<_, ArtifactSetEntry>(
            "SELECT path, kind, size, sha256, executable, artifact_name FROM artifact_set_entries
             WHERE set_id = ? ORDER BY path",
        )
        .bind(set.id)
        .fetch_all(&mut *tx)
        .await?;
        if inserted {
            for (index, entry) in entries.iter().enumerate() {
                // Internal names cannot collide with declared names or other attempts.
                let internal = (entry.kind == ArtifactEntryKind::File)
                    .then(|| format!(".set-{}-{index}", set.id));
                sqlx::query(
                    "INSERT INTO artifact_set_entries
                             (set_id, path, kind, size, sha256, executable, artifact_name)
                             VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(set.id)
                .bind(&entry.path)
                .bind(entry.kind)
                .bind(entry.size)
                .bind(&entry.sha256)
                .bind(entry.executable)
                .bind(internal)
                .execute(&mut *tx)
                .await?;
            }
        } else if existing.len() != entries.len()
            || existing.iter().zip(entries).any(|(a, b)| {
                a.path != b.path
                    || a.kind != b.kind
                    || a.size != b.size
                    || a.sha256 != b.sha256
                    || a.executable != b.executable
            })
        {
            return Err(StoreError::Invalid("重试清单与已有产物集不一致".into()));
        }
        let actual = sqlx::query_as::<_, ArtifactSetEntry>(
            "SELECT path, kind, size, sha256, executable, artifact_name FROM artifact_set_entries
             WHERE set_id = ? ORDER BY path",
        )
        .bind(set.id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((set, actual))
    }

    /// 查询当前构建、任务、attempt 的清单文件，仅允许 pending 集合写入。
    pub async fn pending_set_file(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        artifact_name: &str,
    ) -> Result<Option<ArtifactSetEntry>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT e.path, e.kind, e.size, e.sha256, e.executable, e.artifact_name FROM artifact_set_entries e
                           JOIN artifact_sets s ON s.id = e.set_id
                           WHERE e.artifact_name = ? AND s.state = 'pending'
                             AND s.build_id = ? AND s.job_id = ? AND s.attempt = ? AND e.kind = 'file'",
        )
        .bind(artifact_name)
        .bind(build_id).bind(job_id).bind(attempt)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 列出已经完整发布的目录产物，包括历史 attempt。
    pub async fn list_sets(&self, build_id: i64) -> Result<Vec<ArtifactSetRow>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT id, build_id, job_id, attempt, name, state, created_at
                           FROM artifact_sets WHERE build_id = ? AND state = 'ready' ORDER BY id",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按稳定 ID 查询集合；调用方负责归属授权。
    pub async fn set(&self, set_id: i64) -> Result<Option<ArtifactSetRow>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT id, build_id, job_id, attempt, name, state, created_at
                           FROM artifact_sets WHERE id = ?",
        )
        .bind(set_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 按任务 attempt 与声明名查询集合，用于防止文件/目录命名空间碰撞。
    pub async fn set_by_name(
        &self,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<Option<ArtifactSetRow>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT id, build_id, job_id, attempt, name, state, created_at FROM artifact_sets
                           WHERE job_id = ? AND attempt = ? AND name = ?",
        )
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 按相对路径顺序读取不可变清单。
    pub async fn set_entries(&self, set_id: i64) -> Result<Vec<ArtifactSetEntry>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT path, kind, size, sha256, executable, artifact_name
                           FROM artifact_set_entries WHERE set_id = ? ORDER BY path",
        )
        .bind(set_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 在事务内验证全部文件归属、字节数和摘要，然后整体发布。
    pub async fn publish_set(&self, set_id: i64) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let missing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM artifact_set_entries e JOIN artifact_sets s ON s.id = e.set_id
             LEFT JOIN artifacts a
             ON a.name = e.artifact_name AND a.state = 'ready'
                AND a.build_id = s.build_id AND a.job_id = s.job_id AND a.attempt = s.attempt
             WHERE e.set_id = ? AND e.kind = 'file'
               AND (a.id IS NULL OR a.size != e.size OR a.sha256 != e.sha256)",
        )
        .bind(set_id)
        .fetch_one(&mut *tx)
        .await?;
        if missing != 0 {
            return Err(StoreError::Invalid(format!(
                "产物集还有 {missing} 个文件未完成校验"
            )));
        }
        sqlx::query("UPDATE artifact_sets SET state = 'ready' WHERE id = ? AND state = 'pending'")
            .bind(set_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// 从既有池装配（表已由迁移建好）。`retention_days` 为全局保留期天数
    /// （config `[retention]` 合并值；与日志共享 per-build 保留期，ADR-0013）。
    pub fn new(pool: SqlitePool, retention_days: i64) -> Self {
        Self {
            pool,
            retention_days: retention_days.max(1),
        }
    }

    /// 列出一次构建的全部产物（含上传时刻，按名排序）——构建详情页产物
    /// 列表消费（[`ArtifactMetaRepo::list_by_build`] 缝不含时间列，此处是
    /// 查询面冗余列的同层扩展，不破缝契约）。
    pub async fn list_with_created_at(
        &self,
        build_id: i64,
    ) -> Result<Vec<ArtifactMetaEntry>, StoreError> {
        let rows = sqlx::query_as::<_, ArtifactRow>(
            "SELECT build_id, job_id, attempt, backend, state, name, path, size, sha256,
                    created_at FROM artifacts
             WHERE build_id = ? AND state != 'pending' AND name NOT LIKE '.set-%' ORDER BY name",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(ArtifactRow::into_entry)
            .collect::<Result<Vec<_>, StoreError>>()
    }

    /// 含 pending 的按名查询（grant/complete 复用同一 pending 行）。
    pub async fn find_including_pending(
        &self,
        build_id: i64,
        name: &str,
    ) -> Result<Option<ArtifactMeta>, StoreError> {
        let row = sqlx::query_as::<_, ArtifactRow>(
            "SELECT build_id, job_id, attempt, backend, state, name, path, size, sha256,
                    created_at
             FROM artifacts
             WHERE build_id = ? AND name = ?",
        )
        .bind(build_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ArtifactRow::into_meta).transpose()
    }

    /// 查询本次任务 attempt 已签发/发布的其它产物大小之和。
    pub async fn other_upload_bytes(
        &self,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<u64, StoreError> {
        let total: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(size), 0) FROM artifacts
             WHERE job_id = ? AND attempt = ? AND name != ?",
        )
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(total.max(0) as u64)
    }

    /// 记录或替换 multipart 会话（同 build/job/attempt/name 的重试复用一个槽位）。
    pub async fn record_multipart(&self, row: &MultipartUploadRow) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO artifact_multipart_uploads
                (build_id, name, job_id, attempt, object_key, upload_id, size,
                 part_size, completed, expires_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (build_id, job_id, attempt, name) DO UPDATE SET
               job_id = excluded.job_id, attempt = excluded.attempt,
               object_key = excluded.object_key, upload_id = excluded.upload_id,
               size = excluded.size, part_size = excluded.part_size,
               completed = excluded.completed,
               expires_at = excluded.expires_at, created_at = excluded.created_at",
        )
        .bind(row.build_id)
        .bind(&row.name)
        .bind(row.job_id)
        .bind(row.attempt)
        .bind(&row.object_key)
        .bind(&row.upload_id)
        .bind(row.size as i64)
        .bind(row.part_size as i64)
        .bind(row.completed)
        .bind(row.expires_at)
        .bind(crate::store::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 查询一个产物当前的 multipart 会话。
    pub async fn find_multipart(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<Option<MultipartUploadRow>, StoreError> {
        let row = sqlx::query_as::<_, MultipartUploadDbRow>(
            "SELECT build_id, name, job_id, attempt, object_key, upload_id, size,
                    part_size, completed, expires_at
             FROM artifact_multipart_uploads WHERE build_id = ? AND job_id = ? AND attempt = ? AND name = ?",
        )
        .bind(build_id)
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// 列出已过期会话，供后台与请求路径 abort。
    pub async fn list_expired_multipart(
        &self,
        now: i64,
    ) -> Result<Vec<MultipartUploadRow>, StoreError> {
        let rows = sqlx::query_as::<_, MultipartUploadDbRow>(
            "SELECT build_id, name, job_id, attempt, object_key, upload_id, size,
                    part_size, completed, expires_at
             FROM artifact_multipart_uploads WHERE expires_at <= ? ORDER BY expires_at",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 删除已完成/已 abort 的 multipart 会话。
    pub async fn delete_multipart(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM artifact_multipart_uploads WHERE build_id = ? AND job_id = ? AND attempt = ? AND name = ?")
            .bind(build_id)
            .bind(job_id)
            .bind(attempt)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 只清理仍匹配这个过期 upload id 的会话与 pending 行；事务保证二者
    /// 同步移除，避免后台扫描误删并发重新签发的新会话。
    pub async fn cleanup_expired_multipart(
        &self,
        row: &MultipartUploadRow,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let removed = sqlx::query(
            "DELETE FROM artifact_multipart_uploads
             WHERE build_id = ? AND name = ? AND upload_id = ? AND expires_at <= ?",
        )
        .bind(row.build_id)
        .bind(&row.name)
        .bind(&row.upload_id)
        .bind(crate::store::now_ms())
        .execute(&mut *tx)
        .await?;
        if removed.rows_affected() == 1 {
            sqlx::query(
                "DELETE FROM artifacts
                 WHERE build_id = ? AND name = ? AND job_id = ? AND attempt = ?
                   AND state = 'pending'",
            )
            .bind(row.build_id)
            .bind(&row.name)
            .bind(row.job_id)
            .bind(row.attempt)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 标记临时 multipart 对象已完成，允许 complete 请求重试复制与校验。
    pub async fn mark_multipart_completed(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE artifact_multipart_uploads SET completed = 1
             WHERE build_id = ? AND job_id = ? AND attempt = ? AND name = ?",
        )
        .bind(build_id)
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct MultipartUploadDbRow {
    build_id: i64,
    name: String,
    job_id: i64,
    attempt: i32,
    object_key: String,
    upload_id: String,
    size: i64,
    part_size: i64,
    completed: bool,
    expires_at: i64,
}

impl From<MultipartUploadDbRow> for MultipartUploadRow {
    fn from(row: MultipartUploadDbRow) -> Self {
        Self {
            build_id: row.build_id,
            name: row.name,
            job_id: row.job_id,
            attempt: row.attempt,
            object_key: row.object_key,
            upload_id: row.upload_id,
            size: row.size.max(0) as u64,
            part_size: row.part_size.max(0) as u64,
            completed: row.completed,
            expires_at: row.expires_at,
        }
    }
}

impl ArtifactMetaRepo for SqliteArtifactMetaRepo {
    async fn record(&self, meta: &ArtifactMeta) -> Result<(), StoreError> {
        // 带任务归属的行按 (build, job, attempt, name) 唯一；legacy 行继续
        // 保持 (build, name) 覆盖兼容。retention 自落库时刻起保留期（全局
        // 配置，默认 30 天，ADR-0013/B5-T6）。
        let now = crate::store::now_ms();
        // S3 新产物默认永久保留（ADR-0026）；本地仍按全局天数。
        let retention_until = match meta.backend {
            ArtifactBackend::S3 => i64::MAX,
            ArtifactBackend::Local => now + self.retention_days * 24 * 60 * 60 * 1000,
        };
        // 旧调用面没有任务归属，沿用历史 (build, name) 覆盖语义；新任务行
        // 使用完整四元组约束，允许同构建不同任务声明同名产物。
        if meta.job_id.is_none() || meta.attempt.is_none() {
            sqlx::query("DELETE FROM artifacts WHERE build_id = ? AND name = ? AND job_id IS NULL")
                .bind(meta.build_id)
                .bind(&meta.name)
                .execute(&self.pool)
                .await?;
        }
        let query = if meta.job_id.is_some() && meta.attempt.is_some() {
            sqlx::query(
                "INSERT INTO artifacts
                (build_id, job_id, attempt, backend, state, name, path, size, sha256,
                 created_at, retention_until)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (build_id, job_id, attempt, name) DO UPDATE SET
               job_id = excluded.job_id, attempt = excluded.attempt,
               backend = excluded.backend, state = excluded.state,
               path = excluded.path, size = excluded.size, sha256 = excluded.sha256,
               created_at = excluded.created_at, retention_until = excluded.retention_until",
            )
        } else {
            sqlx::query(
                "INSERT INTO artifacts
                (build_id, job_id, attempt, backend, state, name, path, size, sha256,
                 created_at, retention_until)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (build_id, job_id, attempt, name) DO UPDATE SET
               backend = excluded.backend, state = excluded.state,
               path = excluded.path, size = excluded.size, sha256 = excluded.sha256,
               created_at = excluded.created_at, retention_until = excluded.retention_until",
            )
        };
        query
            .bind(meta.build_id)
            .bind(meta.job_id)
            .bind(meta.attempt)
            .bind(meta.backend.as_str())
            .bind(meta.state.as_str())
            .bind(&meta.name)
            .bind(&meta.path)
            .bind(meta.size as i64)
            .bind(&meta.sha256)
            .bind(now)
            .bind(retention_until)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn find(&self, build_id: i64, name: &str) -> Result<Option<ArtifactMeta>, StoreError> {
        let row = sqlx::query_as::<_, ArtifactRow>(
            "SELECT build_id, job_id, attempt, backend, state, name, path, size, sha256,
                    created_at
             FROM artifacts
             WHERE build_id = ? AND name = ? AND state != 'pending'",
        )
        .bind(build_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ArtifactRow::into_meta).transpose()
    }

    async fn list_by_build(&self, build_id: i64) -> Result<Vec<ArtifactMeta>, StoreError> {
        let rows = sqlx::query_as::<_, ArtifactRow>(
            "SELECT build_id, job_id, attempt, backend, state, name, path, size, sha256,
                    created_at
             FROM artifacts
             WHERE build_id = ? AND state != 'pending' ORDER BY name",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(ArtifactRow::into_meta)
            .collect::<Result<Vec<_>, StoreError>>()
    }

    async fn set_state(
        &self,
        build_id: i64,
        name: &str,
        state: ArtifactState,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE artifacts SET state = ? WHERE build_id = ? AND name = ?")
            .bind(state.as_str())
            .bind(build_id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_state_for_job(
        &self,
        build_id: i64,
        job_id: i64,
        attempt: i32,
        name: &str,
        state: ArtifactState,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE artifacts SET state = ?
             WHERE build_id = ? AND job_id = ? AND attempt = ? AND name = ?",
        )
        .bind(state.as_str())
        .bind(build_id)
        .bind(job_id)
        .bind(attempt)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

    /// #121：0019 可直接升级已有行的 0012 schema，且不要求补造任务归属。
    #[tokio::test]
    async fn storage_compat_migration_backfills_legacy_rows() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("内存库");
        sqlx::raw_sql(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE builds (id INTEGER PRIMARY KEY);
             CREATE TABLE jobs (id INTEGER PRIMARY KEY);
             INSERT INTO builds (id) VALUES (7);
             CREATE TABLE artifacts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 build_id INTEGER NOT NULL REFERENCES builds(id),
                 name TEXT NOT NULL,
                 path TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 sha256 TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 retention_until INTEGER NOT NULL,
                 UNIQUE (build_id, name)
             );
             CREATE INDEX idx_artifacts_retention ON artifacts(retention_until);
             INSERT INTO artifacts
                 (build_id, name, path, size, sha256, created_at, retention_until)
             VALUES (7, 'legacy.bin', '7/legacy.bin', 3, 'abc', 10, 20);",
        )
        .execute(&pool)
        .await
        .expect("建旧 schema 与数据");

        sqlx::raw_sql(include_str!("migrations/0019_artifact_storage_compat.sql"))
            .execute(&pool)
            .await
            .expect("升级旧数据");

        let row: (String, Option<i64>, Option<i32>, String) = sqlx::query_as(
            "SELECT backend, job_id, attempt, state FROM artifacts WHERE build_id = 7",
        )
        .fetch_one(&pool)
        .await
        .expect("读迁移结果");
        assert_eq!(row, ("local".into(), None, None, "ready".into()));
    }

    /// 临时库装配：bootstrap（迁移含 0012 artifacts 表）+ 父行（项目/构建）。
    async fn fixture() -> (
        tempfile::TempDir,
        LocalDiskArtifactStore,
        SqliteArtifactMetaRepo,
    ) {
        let dir = tempfile::tempdir().expect("临时数据目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = crate::store::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        sqlx::query("INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at) VALUES ('demo', 'git', 'https://example.com/r', 0, 0)")
            .execute(&pool)
            .await
            .expect("建项目");
        sqlx::query("INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at) VALUES (1, 'release', 1, 'running', 'manual', '{}', 1, '{}', 0)")
            .execute(&pool)
            .await
            .expect("建构建");
        let store = LocalDiskArtifactStore::new(dir.path().join("artifacts"));
        let repo = SqliteArtifactMetaRepo::new(pool, crate::config::DEFAULT_RETENTION_DAYS);
        (dir, store, repo)
    }

    /// 字节流夹具。
    fn bytes_stream(data: &[u8]) -> ByteStream {
        let first = data[..data.len() / 2].to_vec();
        let second = data[data.len() / 2..].to_vec();
        stream::iter(vec![Ok(first), Ok(second)]).boxed()
    }

    /// sha256 hex（与存储侧同算法）。
    fn sha256_hex(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    #[tokio::test]
    async fn store_streams_to_disk_with_sha256_and_meta_path() {
        let (dir, store, repo) = fixture().await;
        let data = b"hello artifact bytes".repeat(100);
        let meta = store
            .store(1, "dist.bin", bytes_stream(&data))
            .await
            .expect("落盘");
        assert_eq!(meta.build_id, 1);
        assert_eq!(meta.name, "dist.bin");
        assert_eq!(meta.path, "1/dist.bin");
        assert_eq!(meta.size, data.len() as u64);
        assert_eq!(meta.sha256, sha256_hex(&data));

        // 磁盘布局：artifacts/<build_id>/<name>，无 .part 残留。
        let file = dir.path().join("artifacts").join("1").join("dist.bin");
        assert_eq!(tokio::fs::read(&file).await.expect("读回"), data);
        let entries = list_dir(&dir.path().join("artifacts").join("1")).await;
        assert_eq!(entries, vec!["dist.bin".to_string()], "无临时文件残留");

        // 元数据落库 round-trip。
        repo.record(&meta).await.expect("record");
        let found = repo.find(1, "dist.bin").await.expect("find");
        assert_eq!(found, Some(meta));
    }

    #[tokio::test]
    async fn open_roundtrips_streaming_bytes() {
        let (dir, store, _repo) = fixture().await;
        let data = vec![7u8; 200_000]; // > 64 KiB，跨块。
        store
            .store(1, "big.bin", bytes_stream(&data))
            .await
            .expect("落盘");
        let mut out = Vec::new();
        let mut stream = store.open(1, "big.bin").await.expect("open");
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("块"));
        }
        assert_eq!(out, data);
        assert!(dir.path().join("artifacts/1/big.bin").exists());
    }

    #[tokio::test]
    async fn store_reupload_same_name_overwrites_atomically() {
        let (_dir, store, repo) = fixture().await;
        store
            .store(1, "app.tar", bytes_stream(b"v1-bytes"))
            .await
            .expect("首传");
        let meta_v2 = store
            .store(1, "app.tar", bytes_stream(b"v2-longer-bytes"))
            .await
            .expect("再传");
        repo.record(&meta_v2).await.expect("record v2");

        let found = repo.find(1, "app.tar").await.expect("find");
        assert_eq!(
            found.as_ref().map(|m| m.sha256.clone()),
            Some(sha256_hex(b"v2-longer-bytes"))
        );
        let rows = repo.list_by_build(1).await.expect("list");
        assert_eq!(rows.len(), 1, "(build, name) 唯一——覆盖非新增");
    }

    #[tokio::test]
    async fn record_is_idempotent_upsert() {
        let (_dir, _store, repo) = fixture().await;
        let meta = ArtifactMeta {
            build_id: 1,
            job_id: None,
            attempt: None,
            backend: ArtifactBackend::Local,
            state: ArtifactState::Ready,
            name: "x".into(),
            path: "1/x".into(),
            size: 3,
            sha256: "abc".into(),
        };
        repo.record(&meta).await.expect("record 1");
        repo.record(&meta).await.expect("record 2（幂等覆盖）");
        assert_eq!(repo.list_by_build(1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_name_from_different_jobs_is_scoped_by_job_and_attempt() {
        let (_dir, store, repo) = fixture().await;
        sqlx::query("INSERT INTO jobs (id, build_id, stage_index, name, status, attempt, labels, timeout_minutes, retry_count, allow_failure) VALUES (11, 1, 0, 'linux', 'succeeded', 1, '[]', 0, 0, 0), (12, 1, 0, 'windows', 'succeeded', 1, '[]', 0, 0, 0)")
            .execute(&repo.pool)
            .await
            .expect("建来源任务");
        let first = store
            .store_for_job(1, 11, 1, "bundle", bytes_stream(b"linux"))
            .await
            .expect("linux 产物");
        let second = store
            .store_for_job(1, 12, 1, "bundle", bytes_stream(b"windows"))
            .await
            .expect("windows 产物");
        let mut first = first;
        first.job_id = Some(11);
        first.attempt = Some(1);
        let mut second = second;
        second.job_id = Some(12);
        second.attempt = Some(1);
        repo.record(&first).await.expect("记录 linux");
        repo.record(&second).await.expect("记录 windows");

        assert_eq!(repo.list_by_build(1).await.unwrap().len(), 2);
        assert_eq!(
            repo.find_for_job(1, 11, 1, "bundle")
                .await
                .unwrap()
                .unwrap()
                .sha256,
            sha256_hex(b"linux")
        );
        assert_eq!(
            repo.find_for_job(1, 12, 1, "bundle")
                .await
                .unwrap()
                .unwrap()
                .sha256,
            sha256_hex(b"windows")
        );
    }

    #[tokio::test]
    async fn open_missing_artifact_is_not_found() {
        let (_dir, store, repo) = fixture().await;
        // ByteStream 无 Debug（expect_err 不可用）：.err() 折 Option 断言。
        #[allow(clippy::err_expect)] // 成功型 ByteStream 未实现 Debug
        let err = store
            .open(1, "absent")
            .await
            .err()
            .expect("缺失产物应 NotFound");
        assert!(matches!(err, StoreError::NotFound(_)), "{err}");
        assert!(repo.find(1, "absent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn names_with_path_segments_are_rejected() {
        let (_dir, store, _repo) = fixture().await;
        for bad in [
            "",
            " ",
            "..",
            ".",
            "a/b",
            "a\\b",
            &"a".repeat(ARTIFACT_NAME_MAX + 1),
            "na\u{0}me",
        ] {
            let err = store
                .store(1, bad, bytes_stream(b"x"))
                .await
                .expect_err("{bad:?} 应拒绝");
            assert!(matches!(err, StoreError::Invalid(_)), "{bad:?}: {err}");
        }
        // 合法名（含空格以外的多字节字符、点号中缀）放行。
        store
            .store(1, "报告-2026.pdf", bytes_stream(b"x"))
            .await
            .expect("多字节名合法");
    }

    #[tokio::test]
    async fn list_by_build_orders_by_name_and_scopes_to_build() {
        let (dir, store, repo) = fixture().await;
        let pool = &repo.pool;
        sqlx::query("INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at) VALUES (1, 'release', 2, 'running', 'manual', '{}', 1, '{}', 0)")
            .execute(pool)
            .await
            .expect("建构建 2");
        for (build, name) in [(1, "b.bin"), (1, "a.bin"), (2, "c.bin")] {
            let meta = store
                .store(build, name, bytes_stream(b"x"))
                .await
                .expect("落盘");
            repo.record(&meta).await.expect("record");
        }
        let names: Vec<String> = repo
            .list_by_build(1)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["a.bin".to_string(), "b.bin".to_string()]);
        assert!(dir.path().join("artifacts/2/c.bin").exists());
    }

    #[tokio::test]
    async fn failed_stream_write_cleans_tmp_and_leaves_no_artifact() {
        let (dir, store, repo) = fixture().await;
        // 中途报错的流：半截写入须清理，不产生可见产物。
        let broken: ByteStream = stream::iter(vec![
            Ok(b"half".to_vec()),
            Err(std::io::Error::other("boom")),
        ])
        .boxed();
        let err = store
            .store(1, "broken.bin", broken)
            .await
            .expect_err("中途报错应失败");
        assert!(matches!(err, StoreError::Io(_)), "{err}");
        let entries = list_dir(&dir.path().join("artifacts").join("1")).await;
        assert!(entries.is_empty(), "无 .part / 半截文件残留：{entries:?}");
        assert!(repo.find(1, "broken.bin").await.unwrap().is_none());
    }
    /// 枚举目录内文件名（tokio ReadDir 逐 next_entry）。
    async fn list_dir(dir: &Path) -> Vec<String> {
        let mut rd = tokio::fs::read_dir(dir).await.expect("枚举");
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.expect("逐项") {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
        out
    }
}
