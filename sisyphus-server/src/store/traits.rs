//! 存储层 trait 缝（ADR-0004：`LogStore` / `ArtifactStore` / 元数据 repo 层）。
//!
//! B2a 只定契约不交付实现：SQLite 日志实现随日志批次（连同 builds/jobs 表）、
//! 磁盘产物实现随产物面批次落在同一缝上。方法面以 ADR-0004/0007/0013
//! 已定语义为限，不为臆测的需求扩面。

// trait 缝保持 AFIT（async fn in trait）形态：Send/dyn 语义随首个真实实现
// 批次（日志/产物面）裁定，不在此臆测收紧。
#![allow(async_fn_in_trait)]

use std::io;

use futures::stream::BoxStream;

use super::StoreError;

/// 日志定位：日志行按 (build, job, attempt, seq) 定位（ADR-0013），
/// seq 在前三者组成的命名空间内单调。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLocation {
    /// 构建 id。
    pub build_id: i64,
    /// 任务 id。
    pub job_id: i64,
    /// 第几次执行（从头重跑占新号；从失败任务重跑沿用原号 attempt+1，ADR-0006）。
    pub attempt: i32,
}

/// 一段日志 chunk：gzip 压缩的事件交织流（输出块 + 步骤生命周期事件），
/// 覆盖自 `start_seq` 起的连续 seq（终点由解压侧确定，ADR-0013）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    /// 本 chunk 首个事件的 seq。
    pub start_seq: u64,
    /// 压缩后的日志事件字节。
    pub compressed: Vec<u8>,
}

/// 产物元数据行（ADR-0004：路径/大小/校验和进库；保留期随日志批次落）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactMeta {
    /// 所属构建。
    pub build_id: i64,
    /// 产物名（任务级声明的上传路径末端名）。
    pub name: String,
    /// 产物键：正斜杠、无盘符的相对路径（ADR-0004：为 v2 对象存储迁移留缝）。
    pub path: String,
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
}

/// 产物字节流：实现无关（本地文件 / v2 对象存储共用），供 HTTP 流式下载。
pub type ByteStream = BoxStream<'static, io::Result<Vec<u8>>>;

/// 构建日志的持久化缝（ADR-0004：Agent 端缓冲批量合并写；ADR-0013：回放+尾随）。
pub trait LogStore {
    /// 批量追加日志 chunk（断线补传时按 start_seq 去重的责任在实现）。
    async fn append(&self, loc: LogLocation, chunks: Vec<LogChunk>) -> Result<(), StoreError>;

    /// 自 `from_seq` 起读取 chunk（SSE `from=<seq>` 回放/续传与整份下载共用，ADR-0013）。
    async fn read_from(&self, loc: LogLocation, from_seq: u64)
        -> Result<Vec<LogChunk>, StoreError>;
}

/// 产物字节存取缝（ADR-0004：本地磁盘布局；ADR-0007：独立 HTTP 端点消费流）。
pub trait ArtifactStore {
    /// 写入一份产物，返回计算出的元数据（路径/大小/SHA-256）——元数据行经
    /// [`ArtifactMetaRepo`] 落库由实现侧组装。
    async fn store(
        &self,
        build_id: i64,
        name: &str,
        content: ByteStream,
    ) -> Result<ArtifactMeta, StoreError>;

    /// 流式打开一份产物（HTTP 下载响应体）。
    async fn open(&self, build_id: i64, name: &str) -> Result<ByteStream, StoreError>;
}

/// 产物元数据行的仓储缝（ADR-0004：元数据进库，与字节存取分属两层）。
pub trait ArtifactMetaRepo {
    /// 记录一行产物元数据（上传完成时）。
    async fn record(&self, meta: &ArtifactMeta) -> Result<(), StoreError>;

    /// 按 (build, name) 查询（下载端点取大小/校验和做响应头）。
    async fn find(&self, build_id: i64, name: &str) -> Result<Option<ArtifactMeta>, StoreError>;

    /// 列出一次构建的全部产物（构建详情页 / 任务下载依赖解析）。
    async fn list_by_build(&self, build_id: i64) -> Result<Vec<ArtifactMeta>, StoreError>;
}
