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
    /// 上传：`job_id` 为本任务行 id、`name` 为产物名、`path` 为工作区内
    /// 源文件（已存在）。
    async fn upload(
        &self,
        job_id: &str,
        name: &str,
        path: &Path,
    ) -> Result<(), ArtifactError>;

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

impl RealArtifactIo {
    /// 以 REST 基址与 token 构造。`api_url` 缺失时调用恒
    /// [`ArtifactError::Unconfigured`]（引导态明确报错，不静默）。
    pub fn new(api_url: Option<String>, token: Option<String>) -> Self {
        Self::with_client(reqwest::Client::new(), api_url, token)
    }

    /// 注入 client 形态（测试直接驱动；与 upgrader 的 `ReqwestDownloader`
    /// 同款可换测缝）。
    pub fn with_client(client: reqwest::Client, api_url: Option<String>, token: Option<String>) -> Self {
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
}

#[async_trait::async_trait]
impl ArtifactIo for RealArtifactIo {
    async fn upload(
        &self,
        job_id: &str,
        name: &str,
        path: &Path,
    ) -> Result<(), ArtifactError> {
        // 引导态校验先行（未配置 api_url/token 时不必碰文件）。
        self.config()?;
        // 源文件流式读（64 KiB 块）→ reqwest Body（chunked 传输，大文件
        // 不整读内存）。
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| ArtifactError::Io(format!("打开 {} 失败：{e}", path.display())))?;
        let body = reqwest::Body::wrap_stream(futures::stream::unfold(file, |mut file| async move {
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
        }));
        let resp = self
            .request(reqwest::Method::POST, &format!("{UPLOAD_ENDPOINT}/{job_id}/{name}"))?
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

/// workspace 相对路径安全拼接：拒绝绝对路径与 `..` 逃逸（声明经 Server
/// 端 model 校验，此处防御性兜底——容器/宿主两后端共用工作区根）。
pub fn safe_join(ws_dir: &Path, relative: &str) -> Result<PathBuf, ArtifactError> {
    let rel = Path::new(relative);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ArtifactError::Io(format!(
            "产物路径须为 workspace 相对路径：{relative}"
        )));
    }
    Ok(ws_dir.join(rel))
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
