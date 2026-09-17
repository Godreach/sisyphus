//! 产物传输（票 #74 / B5-T2，ADR-0004/0006/0007）：Agent 侧经 Server REST
//! 面上传任务产物 / 拉取依赖产物（产物不走 gRPC 流，ADR-0007；与注册/
//! 升级包下载同面——reqwest + Bearer agent token）。
//!
//! - **时机**（由 runner 编排，ADR-0006/0008/0012）：下载依赖在步骤执行前
//!   （工作区就位即可拉）；上传在步骤全部成功、缓存 save 之后、终态上报
//!   之前（槽位占用到上传完成——Server 只认终态上报释放槽位）。
//! - **失败不静默**：上传失败任务上报 failed；下载失败（含「依赖产物尚
//!   不存在」的 404）任务立刻 failed、detail 带服务端消息（清晰报错）。
//! - **流式**：上传请求体逐块读文件（`Body::wrap_stream`，大文件不整读
//!   内存）；下载响应体逐块落盘（先写 `.part` 再原子 rename）。
//!
//! 可测性：HTTP 收在 [`RealArtifactIo`]（[`ArtifactIo`] 缝的实现，注入
//! reqwest client + api_url + token），与 upgrader 的 `Downloader` 缝同款
//! ——runner 持 `Arc<dyn ArtifactIo>`，测试注入 fake（记录调用/阻塞/配定
//! 结果）验证时机与失败映射，不发真请求。

use std::fmt;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// Agent 面上传端点路径前缀（挂 `/api/v1/` 下；与 Server 侧
/// `api::artifacts::agent_upload` 契约）。
pub const UPLOAD_ENDPOINT: &str = "/api/v1/agent/artifacts";

/// 产物传输错误：明确的失败类别（runner 据此组装任务 detail）。
#[derive(Debug)]
pub enum ArtifactError {
    /// 产物面未配置（`api_url` 缺失——通道与 REST 面分置时的引导态）。
    Unconfigured,
    /// 本地文件 IO 失败（源文件缺失 / 落盘失败等）。
    Io(String),
    /// HTTP 传输失败（网络不可达、TLS 失败等）。
    Network(String),
    /// 端点返回非成功（404 含「依赖产物尚不存在」等清晰消息）。
    Rejected {
        /// HTTP 状态码。
        status: u16,
        /// 服务端错误体（统一 JSON 形态的 message；缺省取状态码文本）。
        message: String,
    },
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactError::Unconfigured => {
                write!(f, "产物面未配置（api_url 缺失，无法上传/拉取产物）")
            }
            ArtifactError::Io(e) => write!(f, "文件 IO 失败：{e}"),
            ArtifactError::Network(e) => write!(f, "产物请求失败（网络/传输）：{e}"),
            ArtifactError::Rejected { status, message } => {
                write!(f, "产物端点拒绝（HTTP {status}）：{message}")
            }
        }
    }
}

impl std::error::Error for ArtifactError {}

/// 产物传输缝（票 #74）：上传一份产物 / 下载一份依赖产物。生产实现
/// [`RealArtifactIo`]（reqwest）；runner 测试注入 fake（时机与失败映射的
/// 断言面，不发真请求）。与 upgrader 的 `Downloader` 缝同款。
#[async_trait::async_trait]
pub trait ArtifactIo: Send + Sync {
    /// 整任务传输前提交完整文件大小清单；S3 模式下预检部署级限额。
    async fn preflight(
        &self,
        _job_id: &str,
        _files: &[(String, u64)],
    ) -> Result<(), ArtifactError> {
        Ok(())
    }

    /// 上传：`job_id` 为本任务行 id、`name` 为产物名、`path` 为工作区内
    /// 源文件（已存在）。
    async fn upload(&self, job_id: &str, name: &str, path: &Path) -> Result<(), ArtifactError>;

    /// 上传完整目录清单并在全部文件核验后发布；不支持链接或特殊文件。
    async fn upload_directory(
        &self,
        job_id: &str,
        name: &str,
        path: &Path,
    ) -> Result<(), ArtifactError> {
        let _ = (job_id, name, path);
        Err(ArtifactError::Io("目录产物上传未配置".into()))
    }

    /// 下载依赖：`job_id` 为本任务行 id（Server 侧定位构建）、`source_job`
    /// 为声明的来源任务名、`name` 为产物名、`dest` 为工作区内目标路径。
    async fn download(
        &self,
        job_id: &str,
        source_job: &str,
        name: &str,
        dest: &Path,
    ) -> Result<(), ArtifactError>;
}

/// 生产传输实现：reqwest + Bearer agent token（与注册面同款 client）。
pub struct RealArtifactIo {
    client: reqwest::Client,
    api_url: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadGrant {
    #[serde(default)]
    mode: Option<UploadMode>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    upload_id: Option<String>,
    #[serde(default)]
    part_size: Option<u64>,
    #[serde(default)]
    parts: Vec<UploadPartGrant>,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadPartGrant {
    part_number: u32,
    url: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum UploadMode {
    Single,
    Multipart,
}

#[derive(Debug, Serialize)]
struct CompletedPart {
    part_number: u32,
    etag: String,
}

#[derive(Debug, Deserialize)]
struct DirectoryGrant {
    set: DirectorySet,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct DirectorySet {
    id: i64,
    state: DirectoryState,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DirectoryState {
    Pending,
    Ready,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestEntry {
    path: String,
    kind: EntryKind,
    size: u64,
    sha256: String,
    executable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
}

impl RealArtifactIo {
    async fn download_directory(
        &self,
        job_id: &str,
        manifest: DirectoryGrant,
        dest: &Path,
    ) -> Result<(), ArtifactError> {
        if manifest.set.state != DirectoryState::Ready {
            return Err(ArtifactError::Network("目录产物尚未完整发布".into()));
        }
        validate_manifest(&manifest.entries)?;
        reject_links(dest).await?;
        let parent = dest
            .parent()
            .ok_or_else(|| ArtifactError::Io("目录目标没有父路径".into()))?;
        tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
        let staging = work_path(parent, "staging");
        tokio::fs::create_dir(&staging).await.map_err(io_err)?;
        let result = async {
            for entry in &manifest.entries {
                let path = safe_join(&staging, &entry.path)?;
                if entry.kind == EntryKind::Directory {
                    tokio::fs::create_dir_all(&path).await.map_err(io_err)?;
                    continue;
                }
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
                }
                let resp = self
                    .request(
                        reqwest::Method::GET,
                        &format!("{UPLOAD_ENDPOINT}/{job_id}/sets/{}/file", manifest.set.id),
                    )?
                    .query(&[("path", &entry.path)])
                    .send()
                    .await
                    .map_err(network_err)?;
                let resp = self.follow_download(resp).await?;
                let mut file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .await
                    .map_err(io_err)?;
                let mut stream = resp.bytes_stream();
                let mut hash = sha2::Sha256::new();
                let mut size = 0_u64;
                use tokio::io::AsyncWriteExt;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(network_err)?;
                    size = size.saturating_add(chunk.len() as u64);
                    if size > entry.size {
                        return Err(ArtifactError::Network("目录文件超出清单大小".into()));
                    }
                    hash.update(&chunk);
                    file.write_all(&chunk).await.map_err(io_err)?;
                }
                file.flush().await.map_err(io_err)?;
                drop(file);
                if size != entry.size || format!("{:x}", hash.finalize()) != entry.sha256 {
                    return Err(ArtifactError::Network(format!(
                        "目录文件校验失败：{}",
                        entry.path
                    )));
                }
                #[cfg(unix)]
                if entry.executable {
                    use std::os::unix::fs::PermissionsExt;
                    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .await
                        .map_err(io_err)?;
                }
            }
            // 同目录 rename 整体替换，不把新清单合并到旧目录；失败恢复旧目录。
            reject_links(dest).await?;
            let backup = work_path(parent, "backup");
            let exists = tokio::fs::try_exists(dest).await.map_err(io_err)?;
            if exists {
                tokio::fs::rename(dest, &backup).await.map_err(io_err)?;
            }
            if let Err(e) = tokio::fs::rename(&staging, dest).await {
                if exists {
                    tokio::fs::rename(&backup, dest).await.map_err(io_err)?;
                }
                return Err(io_err(e));
            }
            if exists {
                if tokio::fs::symlink_metadata(&backup)
                    .await
                    .map_err(io_err)?
                    .is_dir()
                {
                    tokio::fs::remove_dir_all(&backup).await.map_err(io_err)?;
                } else {
                    tokio::fs::remove_file(&backup).await.map_err(io_err)?;
                }
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_dir_all(&staging).await;
        }
        result
    }

    async fn follow_download(
        &self,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, ArtifactError> {
        let response = if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| ArtifactError::Network("下载重定向缺少 Location".into()))?;
            self.client
                .get(location)
                .send()
                .await
                .map_err(network_err)?
        } else {
            response
        };
        if !response.status().is_success() {
            return Err(Self::rejection(response).await);
        }
        Ok(response)
    }

    async fn request_upload_grant(
        &self,
        job_id: &str,
        name: &str,
        size: u64,
    ) -> Result<Option<UploadGrant>, ArtifactError> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/{name}/upload-url"),
            )?
            .json(&serde_json::json!({ "size": size }))
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if response.status().as_u16() == 409 {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::rejection(response).await);
        }
        response
            .json()
            .await
            .map(Some)
            .map_err(|e| ArtifactError::Network(e.to_string()))
    }

    /// 以 REST 基址与 token 构造。`api_url` 缺失时调用恒
    /// [`ArtifactError::Unconfigured`]（引导态明确报错，不静默）。
    pub fn new(api_url: Option<String>, token: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("构造产物 HTTP 客户端");
        Self::with_client(client, api_url, token)
    }

    /// 注入 client 形态（测试直接驱动；与 upgrader 的 `ReqwestDownloader`
    /// 同款可换测缝）。
    pub fn with_client(
        client: reqwest::Client,
        api_url: Option<String>,
        token: Option<String>,
    ) -> Self {
        Self {
            client,
            api_url,
            token,
        }
    }

    /// 拼端点 URL + 鉴权头；`api_url`/`token` 缺失即引导态错误。
    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, ArtifactError> {
        let (base, token) = self.config()?;
        Ok(self
            .client
            .request(method, format!("{}{path}", base.trim_end_matches('/')))
            .bearer_auth(token))
    }

    /// 取 REST 基址与 token；缺失即 [`ArtifactError::Unconfigured`]。
    fn config(&self) -> Result<(&str, &str), ArtifactError> {
        let base = self.api_url.as_deref().ok_or(ArtifactError::Unconfigured)?;
        let token = self.token.as_deref().ok_or(ArtifactError::Unconfigured)?;
        Ok((base, token))
    }

    /// 非成功响应 → [`ArtifactError::Rejected`]（读统一 JSON 错误体的
    /// message——「依赖产物尚不存在」等清晰消息透传给任务 detail）。
    async fn rejection(resp: reqwest::Response) -> ArtifactError {
        let status = resp.status().as_u16();
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
        ArtifactError::Rejected { status, message }
    }

    async fn upload_via_server(
        &self,
        job_id: &str,
        name: &str,
        path: &Path,
    ) -> Result<(), ArtifactError> {
        let body = file_body(path).await?;
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/{name}"),
            )?
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Self::rejection(resp).await);
        }
        Ok(())
    }

    async fn upload_part_with_retry(
        &self,
        path: &Path,
        part_size: u64,
        total_size: u64,
        part: UploadPartGrant,
    ) -> Result<CompletedPart, ArtifactError> {
        let offset = u64::from(part.part_number.saturating_sub(1)) * part_size;
        let len = total_size.saturating_sub(offset).min(part_size);
        let mut last_error = String::new();
        for _attempt in 1..=3 {
            let body = file_body_range(path, offset, len).await?;
            match self
                .client
                .put(&part.url)
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let etag = resp
                        .headers()
                        .get(reqwest::header::ETAG)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| {
                            ArtifactError::Network(format!(
                                "multipart 分片 {} 响应缺少 ETag",
                                part.part_number
                            ))
                        })?;
                    return Ok(CompletedPart {
                        part_number: part.part_number,
                        etag,
                    });
                }
                Ok(resp) => {
                    last_error = format!("HTTP {}", resp.status());
                }
                Err(e) => {
                    last_error = e.to_string();
                }
            }
        }
        Err(ArtifactError::Network(format!(
            "multipart 分片 {} 重试耗尽：{last_error}",
            part.part_number
        )))
    }

    async fn upload_multipart(
        &self,
        job_id: &str,
        name: &str,
        path: &Path,
        size: u64,
        grant: &mut UploadGrant,
    ) -> Result<Vec<CompletedPart>, ArtifactError> {
        let part_size = grant
            .part_size
            .filter(|v| *v > 0)
            .ok_or_else(|| ArtifactError::Network("multipart grant 缺少 part_size".into()))?;
        if grant.parts.is_empty() {
            return Err(ArtifactError::Network(
                "multipart grant 缺少分片 URL".into(),
            ));
        }
        let part_numbers = grant
            .parts
            .iter()
            .map(|part| part.part_number)
            .collect::<Vec<_>>();
        let mut completed = Vec::with_capacity(part_numbers.len());
        let mut refresh_at = std::time::Instant::now()
            + std::time::Duration::from_secs(grant.expires_in.saturating_sub(30).max(0) as u64);
        for (index, batch) in part_numbers.chunks(4).enumerate() {
            if index > 0 && std::time::Instant::now() >= refresh_at {
                let fresh = self
                    .request_upload_grant(job_id, name, size)
                    .await?
                    .ok_or_else(|| ArtifactError::Network("multipart 会话已结束".into()))?;
                if fresh.mode != Some(UploadMode::Multipart)
                    || fresh.upload_id != grant.upload_id
                    || fresh.part_size != Some(part_size)
                    || fresh.parts.len() != part_numbers.len()
                {
                    return Err(ArtifactError::Network(
                        "multipart 会话已改变，请重新上传".into(),
                    ));
                }
                *grant = fresh;
                refresh_at = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        grant.expires_in.saturating_sub(30).max(0) as u64
                    );
            }
            let parts = batch
                .iter()
                .map(|number| {
                    grant
                        .parts
                        .iter()
                        .find(|part| part.part_number == *number)
                        .cloned()
                        .ok_or_else(|| {
                            ArtifactError::Network(format!("multipart grant 缺少分片 {number}"))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let results = futures::stream::iter(parts.into_iter().map(|part| async move {
                self.upload_part_with_retry(path, part_size, size, part)
                    .await
            }))
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
            for result in results {
                completed.push(result?);
            }
        }
        completed.sort_by_key(|part| part.part_number);
        Ok(completed)
    }
}

#[async_trait::async_trait]
impl ArtifactIo for RealArtifactIo {
    async fn preflight(&self, job_id: &str, files: &[(String, u64)]) -> Result<(), ArtifactError> {
        let files = files
            .iter()
            .map(|(name, size)| serde_json::json!({ "name": name, "size": size }))
            .collect::<Vec<_>>();
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/preflight"),
            )?
            .json(&serde_json::json!({ "files": files }))
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Self::rejection(resp).await);
        }
        Ok(())
    }

    async fn upload(&self, job_id: &str, name: &str, path: &Path) -> Result<(), ArtifactError> {
        // 引导态校验先行（未配置 api_url/token 时不必碰文件）。
        self.config()?;
        reject_links(path).await?;
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|e| ArtifactError::Io(format!("读取 {} 失败：{e}", path.display())))?
            .len();
        let sha256 = sha256_file(path).await?;

        let Some(mut grant) = self.request_upload_grant(job_id, name, size).await? else {
            return self.upload_via_server(job_id, name, path).await;
        };
        let multipart = grant.mode == Some(UploadMode::Multipart);
        let completed_parts = if multipart {
            self.upload_multipart(job_id, name, path, size, &mut grant)
                .await?
        } else {
            let url = grant
                .url
                .as_deref()
                .ok_or_else(|| ArtifactError::Network("签发响应缺少 url".into()))?;
            let put = self
                .client
                .put(url)
                .header(reqwest::header::CONTENT_LENGTH, size)
                .body(file_body(path).await?)
                .send()
                .await
                .map_err(|e| ArtifactError::Network(e.to_string()))?;
            if !put.status().is_success() {
                return Err(ArtifactError::Network(format!(
                    "直传 PUT 失败：HTTP {}",
                    put.status()
                )));
            }
            Vec::new()
        };
        let complete_body = if multipart {
            serde_json::json!({
                "size": size,
                "sha256": sha256,
                "upload_id": grant.upload_id,
                "parts": completed_parts,
            })
            .to_string()
        } else {
            serde_json::json!({ "size": size, "sha256": sha256 }).to_string()
        };
        let complete = self
            .request(
                reqwest::Method::POST,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/{name}/complete"),
            )?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(complete_body)
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if !complete.status().is_success() {
            return Err(Self::rejection(complete).await);
        }
        Ok(())
    }

    async fn upload_directory(
        &self,
        job_id: &str,
        name: &str,
        path: &Path,
    ) -> Result<(), ArtifactError> {
        self.config()?;
        let entries = collect_directory(path).await?;
        let payload = serde_json::json!({"name": name, "entries": entries.iter().map(|e| serde_json::json!({
            "path": e.path, "kind": e.kind, "size": e.size, "sha256": e.sha256, "executable": e.executable
        })).collect::<Vec<_>>() });
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/sets"),
            )?
            .json(&payload)
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Self::rejection(resp).await);
        }
        let grant: DirectoryGrant = resp
            .json()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if grant.entries.len() != entries.len() {
            return Err(ArtifactError::Network("产物集响应清单不完整".into()));
        }
        for source in &entries {
            let entry = grant
                .entries
                .iter()
                .find(|e| e.path == source.path)
                .filter(|e| {
                    e.kind == source.kind
                        && e.size == source.size
                        && e.sha256 == source.sha256
                        && e.executable == source.executable
                })
                .ok_or_else(|| ArtifactError::Network("产物集响应与源清单不一致".into()))?;
            if source.kind != EntryKind::File {
                continue;
            }
            if entry.state.as_deref() == Some("ready") {
                continue;
            }
            let internal = entry
                .artifact_name
                .as_deref()
                .ok_or_else(|| ArtifactError::Network("产物集响应缺少文件键".into()))?;
            reject_links(&source.absolute).await?;
            self.upload(job_id, internal, &source.absolute).await?;
        }
        if grant.set.state == DirectoryState::Ready {
            return Ok(());
        }
        let set_id = grant.set.id;
        let done = self
            .request(
                reqwest::Method::POST,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/sets/{set_id}/publish"),
            )?
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if !done.status().is_success() {
            return Err(Self::rejection(done).await);
        }
        Ok(())
    }

    async fn download(
        &self,
        job_id: &str,
        source_job: &str,
        name: &str,
        dest: &Path,
    ) -> Result<(), ArtifactError> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("{UPLOAD_ENDPOINT}/{job_id}/downloads/{source_job}/{name}"),
            )?
            .send()
            .await
            .map_err(|e| ArtifactError::Network(e.to_string()))?;
        if resp.status().is_success()
            && resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json"))
        {
            let manifest: DirectoryGrant = resp
                .json()
                .await
                .map_err(|e| ArtifactError::Network(e.to_string()))?;
            return self.download_directory(job_id, manifest, dest).await;
        }
        let resp = if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| ArtifactError::Network("下载重定向缺少 Location".into()))?
                .to_string();
            self.client
                .get(loc)
                .send()
                .await
                .map_err(|e| ArtifactError::Network(e.to_string()))?
        } else {
            resp
        };
        if !resp.status().is_success() {
            return Err(Self::rejection(resp).await);
        }
        // 逐块写 .part 再原子 rename（半截下载不可见——与 Server 落盘同款）。
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ArtifactError::Io(format!("建目录失败：{e}")))?;
        }
        let tmp: PathBuf = dest.with_extension(format!(
            "{}.part",
            dest.extension().and_then(|e| e.to_str()).unwrap_or("dat")
        ));
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let write = async {
            let mut file = tokio::fs::File::create(&tmp).await.map_err(io_err)?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| ArtifactError::Network(e.to_string()))?;
                file.write_all(&chunk).await.map_err(io_err)?;
            }
            file.flush().await.map_err(io_err)?;
            Ok::<(), ArtifactError>(())
        };
        match write.await {
            Ok(()) => {
                tokio::fs::rename(&tmp, dest)
                    .await
                    .map_err(|e| ArtifactError::Io(format!("落盘失败：{e}")))?;
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }
}

/// io::Error → [`ArtifactError::Io`]（闭包内多次用，收口一处）。
fn io_err(e: std::io::Error) -> ArtifactError {
    ArtifactError::Io(e.to_string())
}

async fn sha256_file(path: &Path) -> Result<String, ArtifactError> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = open_regular_file(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.map_err(io_err)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn file_body(path: &Path) -> Result<reqwest::Body, ArtifactError> {
    let file = open_regular_file(path).await?;
    Ok(reqwest::Body::wrap_stream(futures::stream::unfold(
        file,
        |mut file| async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 64 * 1024];
            match file.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(bytes::Bytes::from(buf)), file))
                }
                Err(e) => Some((Err(e), file)),
            }
        },
    )))
}

async fn file_body_range(
    path: &Path,
    offset: u64,
    len: u64,
) -> Result<reqwest::Body, ArtifactError> {
    use tokio::io::AsyncSeekExt;
    let mut file = open_regular_file(path).await?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(io_err)?;
    Ok(reqwest::Body::wrap_stream(futures::stream::unfold(
        (file, len),
        |(mut file, remaining)| async move {
            use tokio::io::AsyncReadExt;
            if remaining == 0 {
                return None;
            }
            let mut buf = vec![0u8; remaining.min(64 * 1024) as usize];
            match file.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((
                        Ok(bytes::Bytes::from(buf)),
                        (file, remaining.saturating_sub(n as u64)),
                    ))
                }
                Err(e) => Some((Err(e), (file, 0))),
            }
        },
    )))
}

/// workspace 相对路径安全拼接：拒绝根路径与 `..` 逃逸（声明经 Server
/// 端 model 校验，此处防御性兜底——容器/宿主两后端共用工作区根）。
///
/// 用 [`Path::has_root`] 而非 [`Path::is_absolute`] 判根：Windows 上裸
/// `/etc/passwd`、`\foo` 这类以分隔符开头的路径 `is_absolute` 为 false
/// （绝对路径要求盘符 `C:\` 或 UNC），会漏过拦截并被 `join` 替换掉
/// workspace 前缀逃逸；`has_root` 对盘符/裸分隔符/UNC 一律判 true，跨
/// 平台一致拒绝。
pub fn safe_join(ws_dir: &Path, relative: &str) -> Result<PathBuf, ArtifactError> {
    let rel = Path::new(relative);
    if relative.is_empty()
        || rel.has_root()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(ArtifactError::Io(format!(
            "产物路径须为 workspace 相对路径：{relative}"
        )));
    }
    Ok(ws_dir.join(rel))
}

#[derive(Debug, Clone)]
struct DirectoryEntry {
    path: String,
    kind: EntryKind,
    size: u64,
    sha256: String,
    executable: bool,
    absolute: PathBuf,
}

async fn collect_directory(root: &Path) -> Result<Vec<DirectoryEntry>, ArtifactError> {
    reject_links(root).await?;
    if !tokio::fs::symlink_metadata(root)
        .await
        .map_err(io_err)?
        .is_dir()
    {
        return Err(ArtifactError::Io("目录产物根不是普通目录".into()));
    }
    let mut out = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    let mut files = 0;
    while let Some(current) = directories.pop() {
        reject_links(&current).await?;
        let mut rd = tokio::fs::read_dir(&current).await.map_err(io_err)?;
        while let Some(item) = rd.next_entry().await.map_err(io_err)? {
            let absolute = item.path();
            reject_links(&absolute).await?;
            let meta = tokio::fs::symlink_metadata(&absolute)
                .await
                .map_err(io_err)?;
            let relative = absolute
                .strip_prefix(root)
                .map_err(|e| ArtifactError::Io(e.to_string()))?;
            let rel = relative
                .iter()
                .map(|s| {
                    s.to_str()
                        .ok_or_else(|| ArtifactError::Io("目录路径必须为 UTF-8".into()))
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            if meta.is_dir() {
                out.push(DirectoryEntry {
                    path: rel,
                    kind: EntryKind::Directory,
                    size: 0,
                    sha256: String::new(),
                    executable: false,
                    absolute: absolute.clone(),
                });
                directories.push(absolute);
            } else {
                files += 1;
                if files > 10_000 {
                    return Err(ArtifactError::Io("目录产物超过 10000 个文件".into()));
                }
                let executable = {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        meta.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                };
                out.push(DirectoryEntry {
                    path: rel,
                    kind: EntryKind::File,
                    size: meta.len(),
                    sha256: sha256_file(&absolute).await?,
                    executable,
                    absolute,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    validate_manifest(
        &out.iter()
            .map(|e| ManifestEntry {
                path: e.path.clone(),
                kind: e.kind,
                size: e.size,
                sha256: e.sha256.clone(),
                executable: e.executable,
                artifact_name: None,
                state: None,
            })
            .collect::<Vec<_>>(),
    )?;
    Ok(out)
}

fn network_err(e: reqwest::Error) -> ArtifactError {
    ArtifactError::Network(e.to_string())
}

fn work_path(parent: &Path, kind: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        ".sisyphus-{kind}-{time}-{}",
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// 拒绝现存路径及其祖先的链接、junction 和特殊文件；允许尚未创建的下载目标。
pub async fn reject_links(path: &Path) -> Result<(), ArtifactError> {
    let mut ancestors = path
        .ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        match tokio::fs::symlink_metadata(ancestor).await {
            Ok(meta) if is_link(&meta) || (!meta.is_file() && !meta.is_dir()) => {
                return Err(ArtifactError::Io(format!(
                    "产物路径含链接或特殊文件：{}",
                    ancestor.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(io_err(e)),
        }
    }
    Ok(())
}

async fn open_regular_file(path: &Path) -> Result<tokio::fs::File, ArtifactError> {
    reject_links(path).await?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path).map_err(io_err)?;
    let meta = file.metadata().map_err(io_err)?;
    if is_link(&meta) || !meta.is_file() {
        return Err(ArtifactError::Io("产物不是普通文件".into()));
    }
    Ok(tokio::fs::File::from_std(file))
}

fn validate_manifest(entries: &[ManifestEntry]) -> Result<(), ArtifactError> {
    let mut paths = std::collections::HashMap::new();
    for entry in entries {
        let valid = !entry.path.is_empty()
            && entry.path.len() <= 1024
            && !entry.path.contains('\\')
            && entry.path.split('/').all(|s| {
                let base = s.split('.').next().unwrap_or("").to_ascii_uppercase();
                !s.is_empty()
                    && s != "."
                    && s != ".."
                    && !s.ends_with(['.', ' '])
                    && !s.chars().any(|c| c.is_control() || ":*?\"<>|".contains(c))
                    && !matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    && !(base.len() == 4
                        && (base.starts_with("COM") || base.starts_with("LPT"))
                        && matches!(base.as_bytes()[3], b'1'..=b'9'))
            });
        if !valid
            || paths
                .insert(entry.path.to_lowercase(), &entry.kind)
                .is_some()
        {
            return Err(ArtifactError::Io(format!(
                "清单路径非法或冲突：{}",
                entry.path
            )));
        }
        match entry.kind {
            EntryKind::File
                if entry.sha256.len() == 64
                    && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            EntryKind::Directory
                if entry.size == 0 && entry.sha256.is_empty() && !entry.executable => {}
            _ => return Err(ArtifactError::Io("清单类型、大小或摘要非法".into())),
        }
    }
    if entries.iter().filter(|e| e.kind == EntryKind::File).count() > 10_000 {
        return Err(ArtifactError::Io("目录产物超过 10000 个文件".into()));
    }
    for entry in entries {
        let mut parent = entry.path.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if paths
                .get(&prefix.to_lowercase())
                .is_some_and(|k| **k == EntryKind::File)
            {
                return Err(ArtifactError::Io("清单文件不能作为父目录".into()));
            }
            parent = prefix;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_relative_and_rejects_escape() {
        let ws = Path::new("/ws");
        assert_eq!(
            safe_join(ws, "out/app.tar").expect("相对路径"),
            PathBuf::from("/ws/out/app.tar")
        );
        assert_eq!(
            safe_join(ws, "a.txt").expect("裸文件名"),
            PathBuf::from("/ws/a.txt")
        );
        // 绝对路径 / .. 逃逸：拒绝。
        assert!(safe_join(ws, "/etc/passwd").is_err());
        assert!(safe_join(ws, "../escape").is_err());
        assert!(safe_join(ws, "a/../../escape").is_err());
        // Windows 盘符绝对路径：拒绝（is_absolute 在此为 true，但同款兜底，
        // 与裸 `/` 路径一并钉死 Windows 逃逸向量）。
        #[cfg(windows)]
        {
            assert!(safe_join(ws, "C:\\windows\\system32").is_err());
            assert!(safe_join(ws, r"\root").is_err());
        }
    }

    #[tokio::test]
    async fn unconfigured_reports_clearly() {
        let io = RealArtifactIo::new(None, Some("sisa_x".into()));
        let err = io
            .upload("1", "a", Path::new("x"))
            .await
            .expect_err("未配置应报错");
        assert!(matches!(err, ArtifactError::Unconfigured), "{err}");
        let err = io
            .download("1", "src", "a", Path::new("x"))
            .await
            .expect_err("未配置应报错");
        assert!(matches!(err, ArtifactError::Unconfigured), "{err}");
    }
}
