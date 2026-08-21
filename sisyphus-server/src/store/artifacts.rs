//! 产物存储（票 #74 / B5-T2，ADR-0004/0006/0007）：本地磁盘字节 + SQLite
//! 元数据两层。
//!
//! - **字节层**（[`LocalDiskArtifactStore`]）：布局 `data/artifacts/<build_id>/
//!   <name>`（ADR-0004）。写入流式落 `.part` 临时文件、边写边算 SHA-256 与
//!   字节数，成功后原子 rename 到最终名（半截文件不可见）；读取按 64 KiB
//!   块流式回放（HTTP 下载响应体）。产物名即磁盘路径段：含路径分隔符或
//!   `..` 的名在 [`validate_artifact_name`] 拒绝（API 层同规则 422，此处
//!   防御性兜底）。
//! - **元数据层**（[`SqliteArtifactMetaRepo`]）：`artifacts` 表一行一份产物，
//!   (build, name) 唯一——重跑/重试同名再传覆盖为最新（`ON CONFLICT DO
//!   UPDATE`，与字节层 rename 覆盖同语义）。`retention_until` 与日志共享
//!   per-build 30 天默认（B5-T6 清理扫描消费，票 #78）。
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
use super::traits::{ArtifactMeta, ArtifactMetaRepo, ArtifactStore, ByteStream};

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
        validate_artifact_name(name)?;
        let dir = self.root.join(build_id.to_string());
        tokio::fs::create_dir_all(&dir).await?;

        // 半截写入不可见：先落 .part 临时文件（同目录保证 rename 原子），
        // 流尽且校验和算完才 rename 到最终名。失败清理临时文件。
        let tmp = dir.join(format!(".{name}.part-{}", now_part_suffix()));
        let meta = write_stream(&tmp, build_id, name, content).await;
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

    async fn open(&self, build_id: i64, name: &str) -> Result<ByteStream, StoreError> {
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

impl SqliteArtifactMetaRepo {
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
        let rows = sqlx::query_as::<_, (i64, String, String, i64, String, i64)>(
            "SELECT build_id, name, path, size, sha256, created_at FROM artifacts
             WHERE build_id = ? ORDER BY name",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(build_id, name, path, size, sha256, created_at)| ArtifactMetaEntry {
                    meta: ArtifactMeta {
                        build_id,
                        name,
                        path,
                        size: size as u64,
                        sha256,
                    },
                    created_at,
                },
            )
            .collect())
    }
}

impl ArtifactMetaRepo for SqliteArtifactMetaRepo {
    async fn record(&self, meta: &ArtifactMeta) -> Result<(), StoreError> {
        // (build, name) 唯一 + 覆盖语义：重跑/重试同名再传以最新为准（与
        // 字节层 rename 覆盖同语义）。retention 自落库时刻起保留期（全局
        // 配置，默认 30 天，ADR-0013/B5-T6）。
        let now = crate::store::now_ms();
        let retention_until = now + self.retention_days * 24 * 60 * 60 * 1000;
        sqlx::query(
            "INSERT INTO artifacts (build_id, name, path, size, sha256, created_at, retention_until)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (build_id, name) DO UPDATE SET
               path = excluded.path, size = excluded.size, sha256 = excluded.sha256,
               created_at = excluded.created_at, retention_until = excluded.retention_until",
        )
        .bind(meta.build_id)
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
        let row = sqlx::query_as::<_, (i64, String, String, i64, String)>(
            "SELECT build_id, name, path, size, sha256 FROM artifacts
             WHERE build_id = ? AND name = ?",
        )
        .bind(build_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map(|(build_id, name, path, size, sha256)| ArtifactMeta {
                build_id,
                name,
                path,
                size: size as u64,
                sha256,
            }),
        )
    }

    async fn list_by_build(&self, build_id: i64) -> Result<Vec<ArtifactMeta>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, String, String, i64, String)>(
            "SELECT build_id, name, path, size, sha256 FROM artifacts
             WHERE build_id = ? ORDER BY name",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(build_id, name, path, size, sha256)| ArtifactMeta {
                build_id,
                name,
                path,
                size: size as u64,
                sha256,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

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
