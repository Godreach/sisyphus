//! 升级包存储（票 #76 / B5-T4，ADR-0017）：本地磁盘字节 + SQLite 元数据两层。
//!
//! 与产物存储（[`crate::store::artifacts`]）同构但更简：升级包是全局资源
//! （按 `package_name` 唯一，不属任何 build），无保留期（管理员手动删旧包，
//! ADR-0017）。包字节布局 `data/upgrade-packages/<package_name>`，元数据进
//! `upgrade_packages` 表（迁移 0014）。
//!
//! - **字节层**（[`LocalDiskUpgradePackageStore`]）：流式落 `.part` 临时文件、
//!   边写边算 SHA-256 与字节数，成功后原子 rename（半截包不可见）；读取按
//!   64 KiB 块流式回放（Agent 下载响应体）。`package_name` 即磁盘路径段，
//!   含路径分隔符或 `..` 的名在 [`validate_package_name`] 拒绝。
//! - **元数据层**（[`UpgradePackageRepo`]）：`upgrade_packages` 表一行一份包，
//!   `package_name` 唯一——同名再传覆盖为最新（`ON CONFLICT DO UPDATE`，与
//!   字节层 rename 覆盖同语义）。`version` 为 JSON（[`AgentVersion`]），由
//!   上传端点按 ADR-0010 文件名规范解析后传入；窗口校验（≥ N-1 且 ≤ Server
//!   版本）在上传端点裁决，窗外拒收，本层只存。
//!
//! Agent 下载端点（`api::upgrade_packages`，agent token 鉴权）消费两层：经
//! [`UpgradePackageRepo::find`] 取 size/sha256 做响应头、
//! [`LocalDiskUpgradePackageStore::open`] 取字节流。

use std::path::{Path, PathBuf};

use futures::StreamExt;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::StoreError;
use super::agents::AgentVersion;
use super::traits::ByteStream;

/// 包名长度上限（`sisyphus-agent-<ver>-<os>-<arch>.tar.gz|.zip` 宽松界）。
pub const PACKAGE_NAME_MAX: usize = 256;

/// 流式读写块大小（64 KiB：与产物/日志同量级）。
const IO_CHUNK: usize = 64 * 1024;

/// 包名校验：非空、无路径分隔符（`/` `\`）、非 `.`/`..`、无控制字符、
/// 长度 <= [`PACKAGE_NAME_MAX`]——包名直接成为磁盘路径段与 URL 段，非法名
/// 在这里与 API 层（409/422）双重拒绝。
pub fn validate_package_name(name: &str) -> Result<(), StoreError> {
    let invalid = |what: &str| StoreError::Invalid(format!("升级包名非法（{what}）：{name}"));
    if name.trim().is_empty() {
        return Err(invalid("空名"));
    }
    if name.len() > PACKAGE_NAME_MAX {
        return Err(invalid(&format!("超过 {PACKAGE_NAME_MAX} 字符")));
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(invalid("不得为路径段"));
    }
    if name.chars().any(char::is_control) {
        return Err(invalid("含控制字符"));
    }
    Ok(())
}

/// 字节层落盘结果（磁盘层只算 size/sha256；version/target 由上传端点按文件名
/// 解析后随元数据落库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePackageBytes {
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
}

/// 升级包元数据行（`upgrade_packages` 表一行的 Rust 形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePackageMeta {
    /// 包名（ADR-0010 规范 `sisyphus-agent-<ver>-<os>-<arch>`，全局唯一）。
    pub package_name: String,
    /// 解析自文件名的版本（JSON 落库）。
    pub version: AgentVersion,
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

/// 本地磁盘升级包字节存储（ADR-0017 布局 `data/upgrade-packages/`）。
#[derive(Debug, Clone)]
pub struct LocalDiskUpgradePackageStore {
    /// 包根（数据目录 `upgrade-packages/`，config 建好布局）。
    root: PathBuf,
}

impl LocalDiskUpgradePackageStore {
    /// 以包根构造（目录由 config 布局保证存在；此处不重复建）。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 包的磁盘路径：`<root>/<package_name>`（调用侧已过名校验）。
    fn package_path(&self, package_name: &str) -> PathBuf {
        self.root.join(package_name)
    }

    /// 流式落盘 + 边写边算 SHA-256/字节数，原子 rename 到最终名（半截包不可见）。
    /// 返回字节数与校验和；version/target 由调用侧（上传端点）按文件名解析后
    /// 随 [`UpgradePackageRepo::record`] 落库。同名再传覆盖（rename 原子覆盖）。
    pub async fn store(
        &self,
        package_name: &str,
        content: ByteStream,
    ) -> Result<UpgradePackageBytes, StoreError> {
        validate_package_name(package_name)?;
        tokio::fs::create_dir_all(&self.root).await?;

        // 半截写入不可见：先落 .part 临时文件（同目录保证 rename 原子），流尽
        // 且校验和算完才 rename 到最终名。失败清理临时文件。
        let tmp = self
            .root
            .join(format!(".{package_name}.part-{}", now_part_suffix()));
        let bytes = write_stream(&tmp, content).await;
        match bytes {
            Ok(bytes) => {
                tokio::fs::rename(&tmp, self.package_path(package_name)).await?;
                Ok(bytes)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    /// 流式打开一份包（HTTP 下载响应体）。不存在返回 [`StoreError::NotFound`]。
    pub async fn open(&self, package_name: &str) -> Result<ByteStream, StoreError> {
        validate_package_name(package_name)?;
        let path = self.package_path(package_name);
        if !tokio::fs::try_exists(&path).await? {
            return Err(StoreError::NotFound(format!("升级包 {package_name} 不存在")));
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

    /// 删除包字节文件（元数据行由 [`UpgradePackageRepo::delete`] 另删）。
    /// 文件不存在视为已删（幂等）。
    pub async fn delete(&self, package_name: &str) -> Result<(), StoreError> {
        validate_package_name(package_name)?;
        match tokio::fs::remove_file(self.package_path(package_name)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 流式写盘 + 边写边算 SHA-256/字节数。
async fn write_stream(tmp: &Path, mut content: ByteStream) -> Result<UpgradePackageBytes, StoreError> {
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
    Ok(UpgradePackageBytes {
        size,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// 临时文件名后缀：毫秒时间戳 + 进程内计数（同毫秒多次上传不撞名即可）。
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

/// SQLite 升级包元数据仓储（`upgrade_packages` 表，迁移 0014）。
#[derive(Debug, Clone)]
pub struct UpgradePackageRepo {
    pool: SqlitePool,
}

impl UpgradePackageRepo {
    /// 从既有池装配（表已由迁移建好）。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 记录一行包元数据（上传完成时）。`package_name` 唯一——同名再传覆盖为
    /// 最新（`ON CONFLICT DO UPDATE`，与字节层 rename 覆盖同语义）。`version`
    /// 以 JSON 文本列存储（与 `agents.agent_version` 同形态）。
    pub async fn record(&self, meta: &UpgradePackageMeta) -> Result<(), StoreError> {
        let version_json = serde_json::to_string(&meta.version).map_err(StoreError::DefinitionJson)?;
        sqlx::query(
            "INSERT INTO upgrade_packages
                (package_name, version, target_os, target_arch, size, sha256, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (package_name) DO UPDATE SET
               version = excluded.version, target_os = excluded.target_os,
               target_arch = excluded.target_arch, size = excluded.size,
               sha256 = excluded.sha256, created_at = excluded.created_at",
        )
        .bind(&meta.package_name)
        .bind(version_json)
        .bind(&meta.target_os)
        .bind(&meta.target_arch)
        .bind(meta.size as i64)
        .bind(&meta.sha256)
        .bind(meta.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按包名查询（下载端点取 size/sha256 做响应头）。不存在返回 `None`。
    pub async fn find(&self, package_name: &str) -> Result<Option<UpgradePackageMeta>, StoreError> {
        let row = sqlx::query_as::<_, (String, String, String, String, i64, String, i64)>(
            "SELECT package_name, version, target_os, target_arch, size, sha256, created_at
             FROM upgrade_packages WHERE package_name = ?",
        )
        .bind(package_name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(map_meta).transpose()
    }

    /// 全部包（管理面清单；按包名排序输出稳定）。
    pub async fn list(&self) -> Result<Vec<UpgradePackageMeta>, StoreError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, i64, String, i64)>(
            "SELECT package_name, version, target_os, target_arch, size, sha256, created_at
             FROM upgrade_packages ORDER BY package_name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_meta).collect()
    }

    /// 删除包元数据行（字节文件由 [`LocalDiskUpgradePackageStore::delete`] 另删）。
    /// 返回 false 表示包不存在。
    pub async fn delete(&self, package_name: &str) -> Result<bool, StoreError> {
        let result =
            sqlx::query("DELETE FROM upgrade_packages WHERE package_name = ?")
                .bind(package_name)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// 元组行 → [`UpgradePackageMeta`]（`version` JSON 列解析；脏 JSON 视为库损坏）。
fn map_meta(
    (package_name, version_json, target_os, target_arch, size, sha256, created_at): (
        String,
        String,
        String,
        String,
        i64,
        String,
        i64,
    ),
) -> Result<UpgradePackageMeta, StoreError> {
    let version: AgentVersion =
        serde_json::from_str(&version_json).map_err(StoreError::DefinitionJson)?;
    Ok(UpgradePackageMeta {
        package_name,
        version,
        target_os,
        target_arch,
        size: size as u64,
        sha256,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

    /// 临时库装配：bootstrap（迁移含 0014 upgrade_packages 表）+ 包存储。
    async fn fixture() -> (
        tempfile::TempDir,
        LocalDiskUpgradePackageStore,
        UpgradePackageRepo,
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
        let store = LocalDiskUpgradePackageStore::new(dir.path().join("upgrade-packages"));
        let repo = UpgradePackageRepo::new(pool);
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

    fn sample_meta(name: &str, bytes: &UpgradePackageBytes, created_at: i64) -> UpgradePackageMeta {
        UpgradePackageMeta {
            package_name: name.into(),
            version: AgentVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
            target_os: "linux".into(),
            target_arch: "x86_64".into(),
            size: bytes.size,
            sha256: bytes.sha256.clone(),
            created_at,
        }
    }

    #[tokio::test]
    async fn store_streams_to_disk_with_sha256() {
        let (dir, store, repo) = fixture().await;
        let name = "sisyphus-agent-1.0.0-linux-x86_64.tar.gz";
        let data = b"upgrade package bytes".repeat(50);
        let bytes = store
            .store(name, bytes_stream(&data))
            .await
            .expect("落盘");
        assert_eq!(bytes.size, data.len() as u64);
        assert_eq!(bytes.sha256, sha256_hex(&data));

        // 磁盘布局：upgrade-packages/<name>，无 .part 残留。
        let file = dir.path().join("upgrade-packages").join(name);
        assert_eq!(tokio::fs::read(&file).await.expect("读回"), data);
        let entries = list_dir(&dir.path().join("upgrade-packages")).await;
        assert_eq!(entries, vec![name.to_string()], "无临时文件残留");

        // 元数据 round-trip（version JSON 落库可读回）。
        let meta = sample_meta(name, &bytes, 1_000);
        repo.record(&meta).await.expect("record");
        let found = repo.find(name).await.expect("find");
        assert_eq!(found, Some(meta));
    }

    #[tokio::test]
    async fn open_roundtrips_streaming_bytes() {
        let (dir, store, _repo) = fixture().await;
        let name = "sisyphus-agent-1.0.0-linux-aarch64.tar.gz";
        let data = vec![9u8; 200_000]; // > 64 KiB，跨块。
        store.store(name, bytes_stream(&data)).await.expect("落盘");
        let mut out = Vec::new();
        let mut stream = store.open(name).await.expect("open");
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("块"));
        }
        assert_eq!(out, data);
        assert!(dir.path().join("upgrade-packages").join(name).exists());
    }

    #[tokio::test]
    async fn reupload_same_name_overwrites_atomically() {
        let (_dir, store, repo) = fixture().await;
        let name = "sisyphus-agent-1.0.0-windows-x86_64.zip";
        let b1 = store
            .store(name, bytes_stream(b"v1-bytes"))
            .await
            .expect("首传");
        repo.record(&sample_meta(name, &b1, 1_000))
            .await
            .expect("record v1");
        let b2 = store
            .store(name, bytes_stream(b"v2-longer-bytes"))
            .await
            .expect("再传");
        repo.record(&sample_meta(name, &b2, 2_000))
            .await
            .expect("record v2");
        let found = repo.find(name).await.expect("find");
        assert_eq!(
            found.as_ref().map(|m| m.sha256.clone()),
            Some(sha256_hex(b"v2-longer-bytes")),
            "覆盖为最新"
        );
        assert_eq!(repo.list().await.unwrap().len(), 1, "唯一——覆盖非新增");
    }

    #[tokio::test]
    async fn list_orders_by_name_and_delete_removes_both_layers() {
        let (_dir, store, repo) = fixture().await;
        for name in [
            "sisyphus-agent-1.0.0-linux-x86_64.tar.gz",
            "sisyphus-agent-1.0.0-macos-aarch64.tar.gz",
            "sisyphus-agent-0.9.0-linux-x86_64.tar.gz",
        ] {
            let bytes = store
                .store(name, bytes_stream(b"pkg"))
                .await
                .expect("落盘");
            repo.record(&sample_meta(name, &bytes, 1_000))
                .await
                .expect("record");
        }
        let names: Vec<String> = repo
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.package_name)
            .collect();
        assert_eq!(names, vec![
            "sisyphus-agent-0.9.0-linux-x86_64.tar.gz".to_string(),
            "sisyphus-agent-1.0.0-linux-x86_64.tar.gz".to_string(),
            "sisyphus-agent-1.0.0-macos-aarch64.tar.gz".to_string(),
        ], "按包名排序");

        // 删一个：元数据行 + 字节文件皆无。
        let gone = "sisyphus-agent-0.9.0-linux-x86_64.tar.gz";
        assert!(repo.delete(gone).await.unwrap(), "删除应命中");
        store.delete(gone).await.expect("字节删");
        assert!(repo.find(gone).await.unwrap().is_none());
        assert_eq!(repo.list().await.unwrap().len(), 2);
        // 再删同一个：false（幂等）。
        assert!(!repo.delete(gone).await.unwrap(), "已删再删 false");
    }

    #[tokio::test]
    async fn open_missing_package_is_not_found() {
        let (_dir, store, repo) = fixture().await;
        #[allow(clippy::err_expect)] // ByteStream 未实现 Debug
        let err = store
            .open("sisyphus-agent-1.0.0-linux-x86_64.tar.gz")
            .await
            .err()
            .expect("缺失包应 NotFound");
        assert!(matches!(err, StoreError::NotFound(_)), "{err}");
        assert!(
            repo.find("sisyphus-agent-1.0.0-linux-x86_64.tar.gz")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn names_with_path_segments_are_rejected() {
        let (_dir, store, _repo) = fixture().await;
        for bad in ["", " ", "..", ".", "a/b", "a\\b", "na\u{0}me"] {
            let err = store
                .store(bad, bytes_stream(b"x"))
                .await
                .expect_err("{bad:?} 应拒绝");
            assert!(matches!(err, StoreError::Invalid(_)), "{bad:?}: {err}");
        }
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
