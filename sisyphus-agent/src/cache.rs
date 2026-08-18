//! Agent 侧缓存（ADR-0012；票 B3-T8 / #54）。
//!
//! 缓存 = 跨构建复用的再生数据快照：`<缓存根>/<pipeline>/<清洗后 key>/`，与
//! 工作区布局 `<工作区根>/<pipeline>/<job>/` 同构（per-pipeline 命名空间）。
//! pipeline 改名 = 缓存重置（与工作区改名语义对齐），旧命名空间成孤儿由
//! LRU 兜底回收。本模块负责：
//!
//! - **key 处理**：Server 已插值的 `key` 随 [`CacheSpec`] 下发；Agent 侧按声明
//!   顺序拼接 `files` 内容取 sha256 前 12 位 hex、以 `-<hash>` 追加（无 files
//!   分量则不追加）；再做目录名清洗（非 `[A-Za-z0-9._-]` → `_`，复用
//!   [`crate::workspace::sanitize_chars`]）与超长截断（目录名 255 上限，**为
//!   hash 后缀留位**——有 files 时用户段截到 242，保证哈希后缀不被截掉）。
//!   registry 记账与列表/删除匹配用**真完整 key**（含哈希，区分版本）。
//! - **files 缺失 = fail-fast**：声明了 `files` 却在工作区读不到 = 任务立即
//!   失败并报错点名缺哪个文件（typo 症状「永不命中且无报错」不可排查，
//!   ADR-0012）。restore 时硬失败；save 时按「save 失败告警不判败」软处理。
//! - **restore/save 时机**：由 runner（#59）在步骤序贯缝上接通——restore 在
//!   最后一个 checkout 步骤后、其余步骤前（锁文件就位才能算 files 哈希；无
//!   checkout 则首步骤前）；save 仅全部步骤成功后、先于产物上传（本批无
//!   上传传输，即步骤成功后立即 save）；取消/超时/失败一律不 save。
//! - **拷贝与并发**：朴素拷贝（v1）；per-key 文件锁（[`fs4`]，restore 共享读
//!   锁 / save 独占锁）+ `<key>.tmp-<uuid>/` 原子换入顶替；并发 last-writer-
//!   wins。缓存一旦保存即不可变快照——构建期间对 workspace 的写入不穿透
//!   污染缓存（save 走 tmp + rename，rename 后的目录不再被写）。
//! - **LRU + 容量上限**：per-Agent 容量上限（字节，0 = 不限）+ LRU 自动淘汰；
//!   save 后触发淘汰直到回到上限内，**跳过正被读/写锁持有的 key**（[`eviction_order`]
//!   纯函数定顺序、[`Cache::evict`] 逐个做非阻塞锁检查）；单条超过上限直接跳过
//!   保存并告警。
//!
//!   **LRU 时钟**：ADR-0012 原文「时钟 = 最近一次 restore 时间」——**仅 restore
//!   刷新 `last_used`**（[`Cache::restore`] 命中经 [`Cache::touch`]）；save **不**刷新
//!   时钟：新条目 `last_used = 0`（从未 restore，淘汰优先级最旧），re-save 保留既有
//!   `last_used`（最近一次 restore 时刻）。save 的 [`Cache::save`] `just_saved`
//!   排除保证本 save 周期不自逐；一个「存而未被复用」的缓存在下次 save 的淘汰中
//!   即为最旧、先被逐——这是 ADR 的有意语义（未被 restore 的缓存未证其用，磁盘
//!   紧张时先让位给被复用者）。
//! - **失败语义**：restore 拷贝中途失败 = 告警并当 miss 继续（缓存是优化不是
//!   依赖）；save 失败 = 告警不判败；save 时 paths 全部缺失 = 跳过保存告警
//!   （部分存在即存）。
//! - **registry**：`<缓存根>/registry.json`（相对路径 → 大小/最近使用/真名），
//!   tmp + rename 原子写；列表/删除指令读 registry + 落盘核对（dir 不存在则
//!   从 registry 剔除）。
//! - **指令**：[`Handle`] 下行循环收 [`CacheCommand`]——列表 → 构造
//!   [`CacheList`] 经活体上行送出；删除（单 key / 全清）→ 删树（单 key 尊重
//!   锁、被锁跳过；全清不问锁）。与工作区列表/清理同款（ADR-0011/0012）。
//!
//! 上行：列表响应经 [`Cache::set_live`] 注入的活体发送器送出（与 logbuf /
//! workspace / runner 同款：连接期 `run_connection` 注入 `out_tx`，断线置
//! `None`）。单 writer 保写序——响应帧与心跳/日志同一上行通道。
//!
//! 缓存 restore/save 由 Agent 宿主侧在 job 工作区目录上执行——容器任务挂载
//! 同一工作区到 `/sisyphus/workspace`，缓存操作在宿主侧 `ws_dir` 上跑，容器内
//! 无感知（ADR-0018）。故本模块只认宿主侧 `ws_dir`，不区分 host/container。

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sisyphus_proto::agent::{
    CacheCommand, CacheEntry, CacheList, CacheSpec, ChannelMessage,
    cache_command::Kind as CacheKind, channel_message::Kind,
};
use tokio::sync::{RwLock, mpsc};

use crate::ReceiptLog;
use crate::workspace::sanitize_chars;

/// registry 文件名（`<缓存根>/registry.json`）。
const REGISTRY_FILE: &str = "registry.json";
/// per-key 锁文件后缀（`<key 目录>.lock`，与 key 目录同级）。
const LOCK_SUFFIX: &str = ".lock";
/// save 临时目录后缀前缀（`<key 目录>.tmp-<uuid>`，原子换入顶替前落盘处）。
const TMP_PREFIX: &str = ".tmp-";
/// 目录名字节上限（多数文件系统 255）。
const MAX_DIR_NAME_LEN: usize = 255;
/// files 哈希后缀长度：`-` + 12 hex = 13 字节。有 files 时为用户 key 段留位，
/// 保证哈希后缀不被超长截断截掉。
const HASH_SUFFIX_LEN: usize = 13;
/// files 哈希取 sha256 前 12 位 hex（= 前 6 字节）。锁文件变更 → key 变 → miss。
const HASH_HEX_LEN: usize = 12;

// ============================================================
// key 处理（纯函数）
// ============================================================

/// 声明顺序拼接 `files` 内容取 sha256 前 12 位 hex。任一文件缺失 =
/// [`MissingFile`]（restore 时 fail-fast、save 时告警跳过）。
///
/// 顺序敏感：声明顺序即拼接顺序，换序得不同哈希（与 ADR-0012「按声明顺序」一致）。
fn files_hash(workspace: &Path, files: &[String]) -> Result<String, MissingFile> {
    let mut hasher = Sha256::new();
    for f in files {
        let path = workspace.join(f);
        match std::fs::read(&path) {
            Ok(bytes) => hasher.update(&bytes),
            Err(_) => return Err(MissingFile(f.clone())),
        }
    }
    Ok(hex_lower_12(&hasher.finalize()))
}

/// files 哈希读取失败的承载（点名缺失文件）。
#[derive(Debug)]
struct MissingFile(String);

/// sha256 摘要前 6 字节 → 12 位小写 hex。
fn hex_lower_12(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(HASH_HEX_LEN);
    for &b in digest.iter().take(HASH_HEX_LEN / 2) {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// 真完整 key：用户 key + `-` + files 哈希（无 files = 用户 key 原样）。registry
/// 记账与列表/删除匹配的真名——含哈希以区分不同锁文件版本的快照。
fn full_key(user_key: &str, hash: Option<&str>) -> String {
    match hash {
        Some(h) => format!("{user_key}-{h}"),
        None => user_key.to_string(),
    }
}

/// key 目录名：清洗 + 超长截断，**为 files 哈希后缀留位**。有 files 时用户段
/// 截到 `MAX_DIR_NAME_LEN - HASH_SUFFIX_LEN`（242），再拼 `-<hash>`，保证
/// 哈希后缀在 255 上限内不被截掉；无 files 时整段截到 255。清洗后恒为 ASCII，
/// 字节截断安全。
///
/// 注：极罕见的边界——两个不同真 key 的用户段前 242 字符相同但哈希不同时，
/// 目录名撞同（哈希后缀已留位不被截，撞名源于用户段截断）。此时 last-writer-
/// wins（save 互相覆盖、registry 后写覆盖先写），v1 接受（用户 key > 242 字符
/// 且共享前缀的声明不现实）。
fn key_dir_name(user_key: &str, hash: Option<&str>) -> String {
    let sanitized = sanitize_chars(user_key);
    match hash {
        Some(h) => {
            let user_cap = MAX_DIR_NAME_LEN.saturating_sub(HASH_SUFFIX_LEN);
            let mut user_part = sanitized;
            if user_part.len() > user_cap {
                user_part.truncate(user_cap);
            }
            // 哈希恒为 12 位 ASCII hex，与留位对齐；总长 <= 242 + 13 = 255。
            format!("{user_part}-{h}")
        }
        None => {
            let mut s = sanitized;
            if s.len() > MAX_DIR_NAME_LEN {
                s.truncate(MAX_DIR_NAME_LEN);
            }
            s
        }
    }
}

/// pipeline 目录名（清洗段，无截断——pipeline 名通常短；与工作区 pipeline 段
/// 同款，复用 [`sanitize_chars`]）。
fn pipeline_dir_name(pipeline: &str) -> String {
    sanitize_chars(pipeline)
}

// ============================================================
// registry（相对路径 → 条目；原子写）
// ============================================================

/// 缓存 registry：相对路径（`<pipeline 目录>/<key 目录>`）→ 条目。落盘为
/// `<缓存根>/registry.json`，tmp + rename 原子写。内存镜像由 [`Cache`] 持有，
/// 每次变更 persist 一次。
#[derive(Serialize, Deserialize, Default, Clone)]
struct Registry {
    /// 相对路径 → 条目。`BTreeMap` 保稳定序列化顺序（原子写后 diff 友好）。
    entries: BTreeMap<String, RegistryEntry>,
}

/// 单条缓存记账：真名 + 大小 + 最近使用（LRU 时钟）。
#[derive(Serialize, Deserialize, Clone)]
struct RegistryEntry {
    /// 真 pipeline 名（列表分组还原）。
    pipeline: String,
    /// 真完整 key（含 files 哈希；列表展示 + 删除匹配）。
    key: String,
    /// 快照字节（save 时实测 tmp 目录大小）。
    size_bytes: i64,
    /// 最近一次 restore/save 的 Unix 毫秒（LRU 时钟）。
    last_used_at_ms: i64,
}

/// 读 registry（不存在/损坏 = 空，不阻塞——registry 丢失 = 缓存不可发现但
/// 目录仍在，LRU 失效；v1 接受，重新 save 即重建）。
fn load_registry(root: &Path) -> Registry {
    std::fs::read_to_string(root.join(REGISTRY_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 原子写 registry（同目录 tmp → rename；rename 失败清 tmp）。失败记警告不
/// panic——registry 是记账，写失败缓存仍可用（下次变更再写）。
fn persist_registry(root: &Path, registry: &Registry) {
    let path = root.join(REGISTRY_FILE);
    let text = match serde_json::to_string(registry) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "registry 序列化失败");
            return;
        }
    };
    let tmp = root.join(format!("{REGISTRY_FILE}.tmp"));
    if std::fs::write(&tmp, &text).is_err() {
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ============================================================
// 淘汰（LRU 顺序 = 纯函数；锁检查 + 删除 = impure）
// ============================================================

/// LRU 淘汰顺序（纯函数）：按 `last_used_at_ms` 升序（最久未用优先），从总量
/// 超出容量的部分起逐个列入淘汰，直到回到上限内；**不含 `just_saved`**（它
/// 是本次 save 的新条目，last_used 最新，本不应被淘汰——过滤之以免被锁
/// 检查误伤）。`capacity_bytes == 0` = 不限，返回空。返回**拥有**的相对路径
/// （调用方 [`Cache::evict`] 据此删目录 + 出 registry——拥有所有权避免边
/// 删边迭代的借用冲突）。
fn eviction_order(
    entries: &BTreeMap<String, RegistryEntry>,
    capacity_bytes: u64,
    just_saved: &str,
) -> Vec<String> {
    if capacity_bytes == 0 {
        return Vec::new();
    }
    let total: i64 = entries.values().map(|e| e.size_bytes).sum();
    if (total as u64) <= capacity_bytes {
        return Vec::new();
    }
    let mut candidates: Vec<(&String, &RegistryEntry)> = entries
        .iter()
        .filter(|(p, _)| p.as_str() != just_saved)
        .collect();
    candidates.sort_by_key(|(_, e)| e.last_used_at_ms);
    let mut to_evict = Vec::new();
    let mut running = total;
    for (path, entry) in candidates {
        if (running as u64) <= capacity_bytes {
            break;
        }
        to_evict.push(path.clone());
        running -= entry.size_bytes;
    }
    to_evict
}

// ============================================================
// 共享状态（组合根 + run_connection + Handle 共用，Clone）
// ============================================================

/// 缓存共享状态：根 + 容量上限 + 活体上行发送器 + registry 内存镜像。`Clone`
/// ——组合根、`run_connection`（set_live/读 cache_bytes）、cache Handle、runner
/// （restore/save 时机钩子）各持一份共享同一内部。
#[derive(Clone)]
pub struct Cache {
    root: PathBuf,
    capacity_bytes: u64,
    live: Arc<RwLock<Option<mpsc::Sender<ChannelMessage>>>>,
    registry: Arc<std::sync::Mutex<Registry>>,
}

impl Cache {
    /// 以缓存根与容量上限构造：建根目录 + 载入 registry。`capacity_bytes == 0`
    /// = 不限（ADR-0012）。registry 不存在/损坏 = 空（重新 save 重建）。
    pub fn new(root: PathBuf, capacity_bytes: u64) -> Self {
        let _ = std::fs::create_dir_all(&root);
        let registry = load_registry(&root);
        Self {
            root,
            capacity_bytes,
            live: Arc::new(RwLock::new(None)),
            registry: Arc::new(std::sync::Mutex::new(registry)),
        }
    }

    /// 缓存根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 缓存记账总字节（registry 各条 size 之和；心跳 [`DiskUsage`]::cache_bytes
    /// 取此）。锁 registry 短暂求和，不持锁跨 await。
    pub fn cache_bytes(&self) -> i64 {
        self.registry
            .lock()
            .expect("registry 锁")
            .entries
            .values()
            .map(|e| e.size_bytes)
            .sum()
    }

    /// 注入/清除活体上行发送器（`run_connection` 每连接调用；断线置 `None`，
    /// 列表响应仅落日志不外送）。与 logbuf / workspace / runner `set_live` 同款。
    pub async fn set_live(&self, tx: Option<mpsc::Sender<ChannelMessage>>) {
        *self.live.write().await = tx;
    }

    /// 缓存条目目录绝对路径：`<根>/<pipeline 目录>/<key 目录>`。纯路径构造，
    /// 不碰盘。
    fn dir_of(&self, pipeline: &str, user_key: &str, hash: Option<&str>) -> PathBuf {
        self.root
            .join(pipeline_dir_name(pipeline))
            .join(key_dir_name(user_key, hash))
    }

    /// relative 路径（`<pipeline 目录>/<key 目录>`，registry 键）。
    fn rel_of(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.root)
            .ok()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string()
    }

    // --------------------------------------------------------
    // restore / save（spawn_blocking——阻塞拷贝 + 阻塞锁不堵 async 运行时）
    // --------------------------------------------------------

    /// restore：算 files 哈希（缺失 = [`RestoreError::MissingFile`] → runner
    /// fail-fast）→ 共享读锁 → 命中则朴素拷贝 cache → workspace，刷新 last_used。
    /// 拷贝中途失败 = 告警 + 当 miss（[`RestoreOutcome::Miss`]），不判败。未命中
    /// = miss（冷启动照常跑）。
    pub async fn restore(
        &self,
        pipeline: &str,
        spec: &CacheSpec,
        ws_dir: &Path,
    ) -> Result<RestoreOutcome, RestoreError> {
        let cache = self.clone();
        let pipeline = pipeline.to_string();
        let spec = spec.clone();
        let ws = ws_dir.to_path_buf();
        match tokio::task::spawn_blocking(move || cache.restore_blocking(&pipeline, &spec, &ws))
            .await
        {
            Ok(inner) => inner,
            Err(join) => {
                tracing::error!(error = %join, "restore 任务 panic，当 miss 继续");
                Ok(RestoreOutcome::Miss)
            }
        }
    }

    fn restore_blocking(
        &self,
        pipeline: &str,
        spec: &CacheSpec,
        ws: &Path,
    ) -> Result<RestoreOutcome, RestoreError> {
        // 1. files 哈希（fail-fast：缺文件点名）。files 为空 = 无哈希分量。
        let hash = if spec.files.is_empty() {
            None
        } else {
            Some(files_hash(ws, &spec.files).map_err(|m| RestoreError::MissingFile(m.0))?)
        };
        let dir = self.dir_of(pipeline, &spec.key, hash.as_deref());

        // 2. 共享读锁（阻塞——等在途 save 完成，确保读到最终快照而非半截）。
        //    锁文件打不开 = 告警并当 miss（不阻塞构建）。
        let _lock = match open_lock(&dir, /*shared=*/ true) {
            Ok(f) => Some(f),
            Err(()) => {
                tracing::warn!("缓存 restore 取锁失败，当 miss 继续");
                return Ok(RestoreOutcome::Miss);
            }
        };

        // 3. 命中判定（锁内——已见 save 最终态）。
        if !dir.is_dir() {
            return Ok(RestoreOutcome::Miss);
        }

        // 4. 朴素拷贝 cache → workspace（per path；src 缺 = 跳过——partial save
        //    的路径本就缺；拷贝 I/O 失败 = 告警 + 当 miss，不判败）。
        let mut copy_error = false;
        for p in &spec.paths {
            let src = dir.join(p);
            if !src.exists() {
                continue;
            }
            if let Err(e) = copy_tree(&src, &ws.join(p)) {
                tracing::warn!(path = %p, error = %e, "缓存 restore 拷贝失败，当 miss 继续");
                copy_error = true;
                break;
            }
        }
        if copy_error {
            return Ok(RestoreOutcome::Miss);
        }

        // 5. 刷新 LRU 时钟（last_used = now；自愈：registry 无此条则补建）。
        self.touch(&dir, pipeline, &full_key(&spec.key, hash.as_deref()));
        Ok(RestoreOutcome::Hit)
    }

    /// save：算 files 哈希（缺失 = 告警跳过，**不判败**）→ 独占锁 → 写
    /// `<key>.tmp-<uuid>/` → rename 顶替 → registry 记账 + LRU 淘汰。paths 全缺
    /// = 跳过保存告警；单条超上限 = 跳过保存告警；拷贝失败 = 告警不判败。
    /// 取不到独占锁（被读/写占） = 告警跳过（last-writer-wins：先拿到的 save）。
    pub async fn save(&self, pipeline: &str, spec: &CacheSpec, ws_dir: &Path) {
        let cache = self.clone();
        let pipeline = pipeline.to_string();
        let spec = spec.clone();
        let ws = ws_dir.to_path_buf();
        if let Err(join) =
            tokio::task::spawn_blocking(move || cache.save_blocking(&pipeline, &spec, &ws)).await
        {
            tracing::error!(error = %join, "save 任务 panic");
        }
    }

    fn save_blocking(&self, pipeline: &str, spec: &CacheSpec, ws: &Path) {
        // 1. files 哈希（缺失 = 告警跳过，不判败——区别于 restore 的 fail-fast）。
        let hash = if spec.files.is_empty() {
            None
        } else {
            match files_hash(ws, &spec.files) {
                Ok(h) => Some(h),
                Err(MissingFile(f)) => {
                    tracing::warn!(file = %f, "缓存 save：files 缺失，跳过保存（不判败）");
                    return;
                }
            }
        };

        // 2. 存在路径；全缺 = 跳过保存告警（部分存在即存）。
        let existing: Vec<&String> = spec.paths.iter().filter(|p| ws.join(p).exists()).collect();
        if existing.is_empty() {
            tracing::warn!("缓存 save：paths 全缺，跳过保存");
            return;
        }

        // 3. 单条超上限 = 跳过保存告警。估计值先做**重叠路径去重**（`paths` 声明
        //    `["d", "d/sub"]` 时 `d/sub` 已计入 `d`，否则 dir_size 重复计数致误跳）：
        //    排序后丢弃任一祖先已被列入的路径。估计是 copy 前的快速 pre-gate
        //    （避免拷巨量数据再弃）；copy 后还有按 tmp 实测大小的精确 gate（步骤 6.5）。
        if self.capacity_bytes > 0 {
            let estimate: i64 = deduped_paths(ws, &existing)
                .iter()
                .map(|p| dir_size(&ws.join(p)))
                .sum();
            if (estimate as u64) > self.capacity_bytes {
                tracing::warn!(
                    estimate_bytes = estimate,
                    capacity_bytes = self.capacity_bytes,
                    "缓存 save：单条超容量上限，跳过保存"
                );
                return;
            }
        }

        let dir = self.dir_of(pipeline, &spec.key, hash.as_deref());
        // pipeline 目录先建（缓存根下）。
        if let Some(parent) = dir.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(error = %e, "缓存 save：建 pipeline 目录失败，跳过保存");
            return;
        }

        // 4. 独占锁（阻塞——等在途 restore/save 完成）。
        let _lock = match open_lock(&dir, /*shared=*/ false) {
            Ok(f) => f,
            Err(()) => {
                tracing::warn!("缓存 save：取独占锁失败，跳过保存（last-writer-wins）");
                return;
            }
        };

        // 5. 清残留 tmp（上次 save 崩在 rename 前；不同 uuid 互不干扰，但积累
        //    占盘，扫一遍清掉）。
        clean_stale_tmp(&dir);

        // 6. 写 tmp 目录 + 朴素拷贝 workspace → tmp。
        let tmp = tmp_dir(&dir);
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            tracing::warn!(error = %e, "缓存 save：建 tmp 目录失败，跳过保存");
            return;
        }
        for p in &existing {
            if let Err(e) = copy_tree(&ws.join(p), &tmp.join(p)) {
                tracing::warn!(path = %p, error = %e, "缓存 save：拷贝失败，放弃保存（不判败）");
                let _ = std::fs::remove_dir_all(&tmp);
                return;
            }
        }
        let size = dir_size(&tmp);

        // 6.5. 精确 gate：copy 后按 tmp 实测大小判「单条超上限」。估计 pre-gate
        //      已挡明显超限者；此处兜重叠路径去重后仍偏估的边角（如硬链接同文件
        //      在两个兄弟目录各计一次、tmp 拷贝只一份），保证「真正超上限者必跳」。
        if self.capacity_bytes > 0 && (size as u64) > self.capacity_bytes {
            tracing::warn!(
                size_bytes = size,
                capacity_bytes = self.capacity_bytes,
                "缓存 save：单条实际超容量上限，放弃保存"
            );
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }

        // 7. rename 顶替（Unix 原生覆盖；Windows 不能 rename 覆盖现存目录，
        //    独占锁已排它，先删旧再 rename）。
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        if let Err(e) = std::fs::rename(&tmp, &dir) {
            tracing::warn!(error = %e, "缓存 save：rename 顶替失败，放弃保存（不判败）");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }

        // 8. registry 记账 + LRU 淘汰。
        let rel = self.rel_of(&dir);
        {
            let mut reg = self.registry.lock().expect("registry 锁");
            // ADR-0012「时钟 = 最近一次 restore 时间」：save **不**刷新 last_used。
            // 新条目 last_used = 0（从未 restore，淘汰优先级最旧）；re-save 保留既有
            // last_used（最近一次 restore 时刻）。仅 restore 命中经 [`Cache::touch`]
            // 刷新时钟。just_saved 排除保证本 save 周期不自逐。
            let last_used = reg
                .entries
                .get(&rel)
                .map(|e| e.last_used_at_ms)
                .unwrap_or(0);
            reg.entries.insert(
                rel.clone(),
                RegistryEntry {
                    pipeline: pipeline.to_string(),
                    key: full_key(&spec.key, hash.as_deref()),
                    size_bytes: size,
                    last_used_at_ms: last_used,
                },
            );
            persist_registry(&self.root, &reg);
            self.evict(&mut reg, &rel);
            persist_registry(&self.root, &reg);
        }
    }

    /// LRU 淘汰直到回到容量内（save 后触发）。逐个对 [`eviction_order`] 给出的
    /// 候选做非阻塞独占锁检查——**被锁（restore 读 / save 写占）则跳过**
    /// （ADR-0012「淘汰跳过被锁 key」）；拿到锁的删目录 + 出 registry。
    fn evict(&self, registry: &mut Registry, just_saved: &str) {
        let to_evict = eviction_order(&registry.entries, self.capacity_bytes, just_saved);
        for rel in to_evict {
            let dir = self.root.join(&rel);
            // 非阻塞独占锁：被占（读/写）= 跳过。
            let Some(lock) = open_lock_try(&dir) else {
                continue;
            };
            // 锁内删目录 + 出 registry（锁持有到本次循环结束，闭合 restore 竞态）。
            remove_dir_all_best_effort(&dir);
            registry.entries.remove(&rel);
            drop(lock);
        }
    }

    /// 刷新 last_used（命中 restore 时调用）。自愈：registry 无此条则按磁盘
    /// 实测大小补建（rename 后未及记 registry 的崩溃窗口由此兜底）。
    fn touch(&self, dir: &Path, pipeline: &str, key: &str) {
        let rel = self.rel_of(dir);
        let mut reg = self.registry.lock().expect("registry 锁");
        let entry = reg
            .entries
            .entry(rel.clone())
            .or_insert_with(|| RegistryEntry {
                pipeline: pipeline.to_string(),
                key: key.to_string(),
                size_bytes: 0,
                last_used_at_ms: 0,
            });
        let was_missing = entry.size_bytes == 0 && entry.last_used_at_ms == 0;
        entry.pipeline = pipeline.to_string();
        entry.key = key.to_string();
        entry.last_used_at_ms = now_ms();
        if was_missing {
            entry.size_bytes = dir_size(dir);
        }
        persist_registry(&self.root, &reg);
    }

    // --------------------------------------------------------
    // 指令（列表 / 删除）
    // --------------------------------------------------------

    /// 处理一帧缓存指令：列表 → 构造 [`CacheList`] 经活体上行送出；删除 →
    /// 删树（无 ack 帧）。无活体连接时列表帧仅记日志（断线不丢指令，重连
    /// 不重发——列表是查询非状态，UI 重发即可）。
    pub async fn handle(&self, cmd: CacheCommand) {
        match cmd.kind {
            Some(CacheKind::List(_)) => {
                let cache = self.clone();
                let entries = tokio::task::spawn_blocking(move || cache.list())
                    .await
                    .unwrap_or_default();
                self.send_up(ChannelMessage {
                    kind: Some(Kind::CacheList(CacheList { entries })),
                })
                .await;
            }
            Some(CacheKind::Delete(req)) => {
                let cache = self.clone();
                let key = req.key;
                if let Err(join) = tokio::task::spawn_blocking(move || cache.delete(&key)).await {
                    tracing::error!(error = %join, "缓存删除任务 panic");
                }
            }
            None => tracing::warn!("缓存指令缺 kind，忽略"),
        }
    }

    /// 列表：读 registry + 落盘核对（dir 不存在则剔除并 persist），产出
    /// [`CacheEntry`]（真名 / 大小 / 最近使用）。
    fn list(&self) -> Vec<CacheEntry> {
        let mut reg = self.registry.lock().expect("registry 锁");
        let missing: Vec<String> = reg
            .entries
            .iter()
            .filter(|(p, _)| !self.root.join(p).is_dir())
            .map(|(p, _)| p.clone())
            .collect();
        for p in &missing {
            reg.entries.remove(p);
        }
        if !missing.is_empty() {
            persist_registry(&self.root, &reg);
        }
        reg.entries
            .values()
            .map(|e| CacheEntry {
                key: e.key.clone(),
                pipeline: e.pipeline.clone(),
                size_bytes: e.size_bytes,
                last_used_at_ms: e.last_used_at_ms,
            })
            .collect()
    }

    /// 删除：单 key（匹配真完整 key，跨 pipeline 删所有匹配——proto `key` 唯
    /// 一字段使然）/ 全清（`key` 空）。单 key 尊重锁——被读/写占则跳过该条；
    /// 全清不问锁（核弹操作，被占的缓存删到 restore 当 miss）。
    fn delete(&self, key: &str) {
        let (to_remove, locks): (Vec<String>, Vec<std::fs::File>) = {
            let mut reg = self.registry.lock().expect("registry 锁");
            let mut to_remove = Vec::new();
            let mut locks = Vec::new();
            for (path, entry) in reg.entries.iter() {
                if !key.is_empty() && entry.key != key {
                    continue;
                }
                let dir = self.root.join(path);
                if key.is_empty() {
                    // 全清：不问锁。
                    to_remove.push(path.clone());
                } else {
                    // 单 key：try 独占锁，拿到才删（持有到目录删完，闭合竞态窗口）。
                    match open_lock_try(&dir) {
                        Some(f) => {
                            to_remove.push(path.clone());
                            locks.push(f);
                        }
                        None => {
                            tracing::warn!(key = %key, "缓存被锁，跳过删除");
                        }
                    }
                }
            }
            for p in &to_remove {
                reg.entries.remove(p);
            }
            persist_registry(&self.root, &reg);
            (to_remove, locks)
        };
        // registry 锁已释；删目录（单 key 的独占锁仍持有，闭合 restore 竞态）。
        for p in &to_remove {
            remove_dir_all_best_effort(&self.root.join(p));
        }
        drop(locks); // 释放独占锁
    }

    /// 经活体发送器上行一帧（断线 = 无发送器，记日志；单 writer 保写序——
    /// `out_tx` 与心跳/日志同一通道）。
    ///
    /// 先克隆发送器出读锁再 send（与 [`crate::logbuf::LogBuffer::forward_live]
    /// 同款）：避免持读锁期间 `tx.send` 阻塞与 `set_live(None)` 写锁互锁。
    async fn send_up(&self, msg: ChannelMessage) {
        let live = self.live.read().await.clone();
        if let Some(tx) = live {
            if tx.send(msg).await.is_err() {
                tracing::warn!("缓存列表响应发送失败：通道已关闭");
            }
        } else {
            tracing::warn!("无活体连接，缓存列表响应未外送（断线；UI 可重发查询）");
        }
    }
}

/// restore 结果：命中（已拷贝 + 刷新时钟）/ 未命中（冷启动，无动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// 命中缓存并完成拷贝。
    Hit,
    /// 未命中（无条目）或拷贝失败已当 miss。
    Miss,
}

/// restore 错误：仅 files 缺失（fail-fast）。拷贝失败已在内部当 miss 处理，
/// 不进此枚举。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// 声明的 files 锁文件在工作区缺失（点名）。runner 据此 fail-fast 任务。
    MissingFile(String),
}

// ============================================================
// 文件系统助手（锁 / 拷贝 / 大小 / 时间）
// ============================================================

/// per-key 锁文件路径：`<key 目录>.lock`（与 key 目录同级，在 pipeline 目录里）。
fn lock_path(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    Some(dir.parent()?.join(format!("{name}{LOCK_SUFFIX}")))
}

/// 打开锁文件并阻塞加锁（`shared` = 共享读 / `false` = 独占写）。创建即开
/// （不存在则建空锁文件，常驻不删）。打不开或加锁失败 = `Err(())`（调用方
/// 告警 + 当 miss / 跳过）。
fn open_lock(dir: &Path, shared: bool) -> Result<std::fs::File, ()> {
    let path = lock_path(dir).ok_or(())?;
    let parent = path.parent().map(|p| p.to_path_buf()).ok_or(())?;
    if std::fs::create_dir_all(&parent).is_err() {
        return Err(());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|_| ())?;
    let r = if shared {
        FileExt::lock_shared(&file)
    } else {
        FileExt::lock(&file)
    };
    if r.is_err() {
        return Err(());
    }
    Ok(file)
}

/// 非阻塞独占锁（淘汰 / 单 key 删除用）：拿不到（被读/写占）= `None`，不阻塞。
fn open_lock_try(dir: &Path) -> Option<std::fs::File> {
    let path = lock_path(dir)?;
    let parent = path.parent().map(|p| p.to_path_buf())?;
    std::fs::create_dir_all(&parent).ok()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    FileExt::try_lock(&file).ok().map(|()| file)
}

/// save 临时目录路径：`<key 目录>.tmp-<uuid>`（与 key 目录同级）。
fn tmp_dir(dir: &Path) -> PathBuf {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("cache");
    let suffix = tmp_suffix();
    dir.parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{name}{TMP_PREFIX}{suffix}"))
}

/// 清同 key 的残留 tmp 目录（`<key 目录>.tmp-*`）——上次 save 崩在 rename 前的孤儿。
fn clean_stale_tmp(dir: &Path) {
    let Some(parent) = dir.parent() else {
        return;
    };
    let Some(key_name) = dir.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let prefix = format!("{key_name}{TMP_PREFIX}");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 递归拷贝 `src` → `dst`（目录递归、文件覆盖、建父目录；符号链接跳过——
/// v1 朴素拷贝，ADR-0012）。restore 与 save 共用：restore 时 dst 可能已存在
/// （复用工作区的上次构建残留），采合并覆盖语义（不预删——失败时保留旧数据）。
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else if meta.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    } else {
        // 符号链接等其它类型：朴素拷贝跳过（不跟随、不复制链接本身）。
        Ok(())
    }
}

/// 重叠路径去重：`paths` 声明 `["d", "d/sub"]` 时 `d/sub` 已计入 `d`——排序后
/// 丢弃任一祖先已被列入的路径，避免 [`dir_size`] 重复计数致「单条超上限」误跳。
/// 返回去重后的路径（相对 `ws` 的引用切片保留原 `existing` 借用）。
fn deduped_paths<'a>(ws: &Path, existing: &[&'a String]) -> Vec<&'a String> {
    let mut sorted: Vec<&'a String> = existing.to_vec();
    sorted.sort();
    let mut deduped: Vec<&'a String> = Vec::new();
    for p in sorted {
        let pp = ws.join(p);
        // 已列入的祖先若存在，则 p 被其覆盖 → 跳过。
        let covered = deduped.iter().any(|a| pp.starts_with(ws.join(a)));
        if !covered {
            deduped.push(p);
        }
    }
    deduped
}

/// 递归求和目录下所有常规文件字节（尽力而为：读失败/元数据失败跳过）。
/// `path` 本身是文件时返回其大小；是目录时递归求和其下文件；其它（符号链接
/// 等）返回 0。
fn dir_size(path: &Path) -> i64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len() as i64;
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total: i64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                total = total.saturating_add(meta.len() as i64);
            }
        }
    }
    total
}

/// 删树（尽力而为：不存在忽略；失败记警告）。
fn remove_dir_all_best_effort(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "缓存目录已删除"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "缓存目录删除失败"),
    }
}

/// Unix 毫秒时间戳（尽力而为：系统时钟异常回退 0）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// save 临时目录后缀（进程内唯一：纳秒 + 自增计数）。同 key 的并发 save 经独占锁
/// 串行化（锁内单写者），同纳秒的两次取后缀由自增计数区分——纳秒分辨率有限时
/// （同纳秒落两次）不撞名。不同 key 的 tmp 在各自 pipeline 目录里、名前缀不同，亦
/// 不撞。
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
fn tmp_suffix() -> String {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

// ============================================================
// 句柄（下行分派循环 owner）
// ============================================================

/// cache 句柄：持有下行接收端、共享状态与收帧观测。`run` 消费 self，
/// 共享状态经 [`Cache`]（`Clone`）另由组合根 / `run_connection` 持有。
pub struct Handle {
    rx: mpsc::Receiver<ChannelMessage>,
    state: Cache,
    receipts: ReceiptLog,
}

impl Handle {
    /// 以分派接收端、共享状态与收帧观测构造。
    pub fn new(rx: mpsc::Receiver<ChannelMessage>, state: Cache, receipts: ReceiptLog) -> Self {
        Self {
            rx,
            state,
            receipts,
        }
    }

    /// 共享状态（组合根装配后供 `run_connection` set_live / runner restore/save / 测试取用）。
    pub fn state(&self) -> &Cache {
        &self.state
    }

    /// 下行循环：收 `CacheCommand` 即交共享状态处理（列表上行 / 删除删树）。
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            let kind_label = match msg.kind {
                Some(Kind::CacheCmd(_)) => "cache",
                _ => "other",
            };
            self.receipts
                .lock()
                .expect("观测锁")
                .push(kind_label.to_string());
            match msg.kind {
                Some(Kind::CacheCmd(cmd)) => self.state.handle(cmd).await,
                _ => tracing::warn!(?msg, "cache 收到非缓存指令，忽略"),
            }
        }
    }
}

// ============================================================
// 单元测试（纯逻辑 + 真实 FS；TDD 红→绿）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_proto::agent::CacheDeleteRequest;
    use std::time::Duration;

    /// 临时缓存根 + Cache（容量 0 = 不限，纯逻辑/restore/save 用）。
    fn cache_dir() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().expect("临时缓存根");
        let cache = Cache::new(dir.path().to_path_buf(), 0);
        (dir, cache)
    }

    // --- hex / files_hash ---

    #[test]
    fn hex_lower_12_takes_first_six_bytes() {
        // sha256(b"") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let h = hex_lower_12(&Sha256::digest(b""));
        assert_eq!(h, "e3b0c44298fc");
        assert_eq!(h.len(), HASH_HEX_LEN);
    }

    #[test]
    fn files_hash_concatenates_in_declaration_order() {
        let dir = tempfile::tempdir().expect("临时工作区");
        std::fs::write(dir.path().join("a"), b"AAA").unwrap();
        std::fs::write(dir.path().join("b"), b"BBB").unwrap();
        let h_ab = files_hash(dir.path(), &["a".into(), "b".into()]).unwrap();
        let h_ba = files_hash(dir.path(), &["b".into(), "a".into()]).unwrap();
        // 顺序不同 → 哈希不同（声明顺序即拼接顺序）。
        assert_ne!(h_ab, h_ba, "files 顺序敏感");
        assert_eq!(h_ab.len(), HASH_HEX_LEN);
        // 确定性：同序同内容 → 同哈希。
        assert_eq!(
            files_hash(dir.path(), &["a".into(), "b".into()]).unwrap(),
            h_ab
        );
    }

    #[test]
    fn files_hash_missing_file_returns_missing_pointing_at_it() {
        let dir = tempfile::tempdir().expect("临时工作区");
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let err = files_hash(dir.path(), &["a".into(), "Cargo.lock".into()]).unwrap_err();
        assert_eq!(err.0, "Cargo.lock", "点名缺失文件");
    }

    // --- full_key / key_dir_name ---

    #[test]
    fn full_key_appends_hash_or_passthrough() {
        assert_eq!(full_key("rust-deps", None), "rust-deps");
        assert_eq!(
            full_key("rust-deps", Some("abc123def456")),
            "rust-deps-abc123def456"
        );
    }

    #[test]
    fn key_dir_name_sanitizes_and_preserves_hash_suffix() {
        // 无 files：清洗 + 截到 255。
        assert_eq!(key_dir_name("rust-deps", None), "rust-deps");
        assert_eq!(key_dir_name("my cache", None), "my_cache", "空格 → _");
        // 有 files：用户段 + -<hash>；非法字符清洗。
        assert_eq!(
            key_dir_name("my cache", Some("abc123def456")),
            "my_cache-abc123def456"
        );
    }

    #[test]
    fn key_dir_name_reserves_room_for_hash_suffix_on_truncation() {
        // 用户 key 超长 + files：用户段截到 242，哈希后缀完整保留（不被截掉）。
        let long = "a".repeat(300);
        let name = key_dir_name(&long, Some("0123456789ab"));
        assert!(name.len() <= MAX_DIR_NAME_LEN, "目录名 <= 255");
        assert!(name.ends_with("-0123456789ab"), "哈希后缀完整保留：{name}");
        // 用户段被截到 242：242 + 1(-) + 12 = 255。
        assert_eq!(name.len(), MAX_DIR_NAME_LEN);
    }

    #[test]
    fn key_dir_name_no_files_truncates_to_255() {
        let long = "a".repeat(300);
        let name = key_dir_name(&long, None);
        assert_eq!(name.len(), MAX_DIR_NAME_LEN);
        assert!(name.chars().all(|c| c == 'a'));
    }

    // --- registry 原子写 ---

    #[tokio::test]
    async fn registry_persists_and_reloads() {
        let (dir, cache) = cache_dir();
        // 经 save 写一条 registry（capacity 0 = 不限，不淘汰）。
        let ws = tempfile::tempdir().expect("临时工作区");
        std::fs::write(ws.path().join("out"), b"hello").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );
        assert!(cache.root().join("pipe").join("k").is_dir(), "缓存目录已写");
        // registry.json 落盘可读。
        let reg: Registry = serde_json::from_str(
            &std::fs::read_to_string(cache.root().join(REGISTRY_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(reg.entries.len(), 1);
        let e = reg.entries.values().next().unwrap();
        assert_eq!(e.pipeline, "pipe");
        assert_eq!(e.key, "k");
        assert_eq!(e.size_bytes, 5);
        // ADR-0012「时钟 = 最近一次 restore 时间」：save **不**刷新时钟——
        // 仅 save 未 restore 的新条目 last_used = 0（从未 restore）。
        assert_eq!(e.last_used_at_ms, 0, "save 不刷新时钟（从未 restore = 0）");

        // restore 命中刷新时钟为 now（ADR-0012 时钟定义）。
        let ws2 = tempfile::tempdir().expect("restore 工作区");
        cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "k".into(),
                    paths: vec!["out".into()],
                    files: vec![],
                },
                ws2.path(),
            )
            .unwrap();
        let reg: Registry = serde_json::from_str(
            &std::fs::read_to_string(cache.root().join(REGISTRY_FILE)).unwrap(),
        )
        .unwrap();
        let e = reg.entries.values().next().unwrap();
        assert!(e.last_used_at_ms > 0, "restore 命中刷新时钟");

        // 重新装载：新 Cache 读到同一条 registry。
        let cache2 = Cache::new(dir.path().to_path_buf(), 0);
        assert_eq!(cache2.cache_bytes(), 5, "重装载 registry");
    }

    // --- restore / save 往返（真实 FS）---

    #[tokio::test]
    async fn save_then_restore_roundtrip_hits_and_copies_files() {
        let (_dir, cache) = cache_dir();
        let ws_a = tempfile::tempdir().expect("工作区 A");
        std::fs::write(ws_a.path().join("out"), b"hello").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws_a.path(),
        );

        // 另一工作区 restore → 命中 + 文件出现。
        let ws_b = tempfile::tempdir().expect("工作区 B");
        let outcome = cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "k".into(),
                    paths: vec!["out".into()],
                    files: vec![],
                },
                ws_b.path(),
            )
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::Hit);
        assert_eq!(
            std::fs::read(ws_b.path().join("out")).unwrap(),
            b"hello",
            "restore 拷贝了缓存内容"
        );
    }

    #[tokio::test]
    async fn restore_miss_when_no_cache_entry() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        let outcome = cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "k".into(),
                    paths: vec!["out".into()],
                    files: vec![],
                },
                ws.path(),
            )
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::Miss);
    }

    /// AC（ADR-0012「restore 拷贝中途失败 = 告警并当 miss 继续」）：命中缓存但
    /// 拷贝失败（目标 `out` 已是目录，文件拷入目录路径失败）→ 当 miss，不判败、
    /// 不刷新 last_used。
    #[tokio::test]
    async fn restore_copy_failure_treated_as_miss() {
        let (_dir, cache) = cache_dir();
        // seed 缓存：pipe/k 含 out（文件）。
        let seed = tempfile::tempdir().expect("seed 工作区");
        std::fs::write(seed.path().join("out"), b"x").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            seed.path(),
        );
        // 工作区的 out 已是目录 → 文件拷入目录路径失败 → 当 miss。
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::create_dir_all(ws.path().join("out")).unwrap();
        let outcome = cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "k".into(),
                    paths: vec!["out".into()],
                    files: vec![],
                },
                ws.path(),
            )
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::Miss, "拷贝失败当 miss，不判败");
    }

    #[tokio::test]
    async fn restore_with_files_hash_hit_only_when_lockfile_matches() {
        let (_dir, cache) = cache_dir();
        // 用 files 哈希 save：key 含锁文件内容哈希。
        let ws_a = tempfile::tempdir().expect("工作区 A");
        std::fs::write(ws_a.path().join("out"), b"built").unwrap();
        std::fs::write(ws_a.path().join("Cargo.lock"), b"lock-v1").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "rust".into(),
                paths: vec!["out".into()],
                files: vec!["Cargo.lock".into()],
            },
            ws_a.path(),
        );

        // 同锁文件内容 restore → 命中（哈希后缀一致）。
        let ws_b = tempfile::tempdir().expect("工作区 B");
        std::fs::write(ws_b.path().join("Cargo.lock"), b"lock-v1").unwrap();
        let outcome = cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "rust".into(),
                    paths: vec!["out".into()],
                    files: vec!["Cargo.lock".into()],
                },
                ws_b.path(),
            )
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::Hit);
        assert_eq!(std::fs::read(ws_b.path().join("out")).unwrap(), b"built");

        // 锁文件变更 → 哈希后缀变 → 不命中（自动 miss）。
        let ws_c = tempfile::tempdir().expect("工作区 C");
        std::fs::write(ws_c.path().join("Cargo.lock"), b"lock-v2-CHANGED").unwrap();
        let outcome = cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "rust".into(),
                    paths: vec!["out".into()],
                    files: vec!["Cargo.lock".into()],
                },
                ws_c.path(),
            )
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::Miss, "锁文件变更 = miss");
        assert!(!ws_c.path().join("out").exists(), "未命中不拷贝");
    }

    #[tokio::test]
    async fn restore_missing_files_is_failfast_with_named_file() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        // 声明 files 但 Cargo.lock 不存在 → fail-fast 点名。
        let err = cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "rust".into(),
                    paths: vec!["out".into()],
                    files: vec!["Cargo.lock".into()],
                },
                ws.path(),
            )
            .unwrap_err();
        assert_eq!(err, RestoreError::MissingFile("Cargo.lock".into()));
    }

    #[tokio::test]
    async fn save_with_missing_files_skips_not_fails() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("out"), b"x").unwrap();
        // files 缺失 → save 跳过（不写缓存、不 panic、registry 空）。
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "rust".into(),
                paths: vec!["out".into()],
                files: vec!["Cargo.lock".into()],
            },
            ws.path(),
        );
        assert!(cache.list().is_empty(), "files 缺失不保存任何缓存");
        assert_eq!(cache.cache_bytes(), 0);
    }

    #[tokio::test]
    async fn save_all_paths_missing_skips() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        // paths 全不存在 → 跳过保存。
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["nope".into()],
                files: vec![],
            },
            ws.path(),
        );
        assert_eq!(cache.cache_bytes(), 0, "全缺不保存");
    }

    #[tokio::test]
    async fn save_partial_paths_stores_existing() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("a"), b"AA").unwrap();
        // b 缺、a 存在 → 存在即存。
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["a".into(), "b".into()],
                files: vec![],
            },
            ws.path(),
        );
        assert_eq!(cache.cache_bytes(), 2, "只存存在的 a");
        // restore 后 a 出现、b 仍缺（partial save 语义）。
        let ws2 = tempfile::tempdir().expect("工作区 B");
        cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "k".into(),
                    paths: vec!["a".into(), "b".into()],
                    files: vec![],
                },
                ws2.path(),
            )
            .unwrap();
        assert_eq!(std::fs::read(ws2.path().join("a")).unwrap(), b"AA");
        assert!(!ws2.path().join("b").exists());
    }

    // --- LRU 淘汰顺序（纯函数 + 真实 FS）---

    #[test]
    fn eviction_order_picks_oldest_until_under_capacity() {
        let mut entries = BTreeMap::new();
        // 三条：最老 → 最新。
        entries.insert(
            "p/old".into(),
            RegistryEntry {
                pipeline: "p".into(),
                key: "old".into(),
                size_bytes: 40,
                last_used_at_ms: 100,
            },
        );
        entries.insert(
            "p/mid".into(),
            RegistryEntry {
                pipeline: "p".into(),
                key: "mid".into(),
                size_bytes: 30,
                last_used_at_ms: 200,
            },
        );
        entries.insert(
            "p/new".into(),
            RegistryEntry {
                pipeline: "p".into(),
                key: "new".into(),
                size_bytes: 30,
                last_used_at_ms: 300,
            },
        );
        // 总 100，上限 60 → 需淘汰 40。最老的 old（40）恰好补足 → 只淘汰 old。
        let order = eviction_order(&entries, 60, "p/new");
        assert_eq!(
            order.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["p/old"]
        );

        // 上限 20 → 淘汰 old(40) 还不够（剩 60 > 20）→ 再淘汰 mid(30)（剩 30 > 20）
        // → 再淘汰... 但 new 排除。候选只剩 old+mid。
        let order = eviction_order(&entries, 20, "p/new");
        assert_eq!(
            order.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["p/old", "p/mid"],
            "按 last_used 升序淘汰直到回到上限"
        );

        // 容量 0 = 不限 → 空淘汰列表。
        assert!(eviction_order(&entries, 0, "p/new").is_empty());
        // 总量未超上限 → 空列表。
        assert!(eviction_order(&entries, 100, "p/new").is_empty());
    }

    #[test]
    fn eviction_order_excludes_just_saved() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "p/just".into(),
            RegistryEntry {
                pipeline: "p".into(),
                key: "just".into(),
                size_bytes: 100,
                last_used_at_ms: 1, // 最老，但它是 just_saved → 排除
            },
        );
        entries.insert(
            "p/other".into(),
            RegistryEntry {
                pipeline: "p".into(),
                key: "other".into(),
                size_bytes: 10,
                last_used_at_ms: 2,
            },
        );
        // 总 110，上限 50 → 淘汰 other（10）仍超（100 > 50），但 just 排除 → 无更多候选。
        let order = eviction_order(&entries, 50, "p/just");
        assert_eq!(
            order.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["p/other"]
        );
    }

    #[tokio::test]
    async fn save_evicts_oldest_until_under_capacity() {
        let dir = tempfile::tempdir().expect("临时缓存根");
        // 容量 6 字节：存得下三条小缓存里的两条。
        let cache = Cache::new(dir.path().to_path_buf(), 6);
        let mk = |key: &str, content: &[u8], ts: i64| {
            let ws = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("out"), content).unwrap();
            // 直接造 registry 条目（绕过 save 的 now_ms，可控时钟）。
            let mut reg = cache.registry.lock().unwrap();
            let dir_name = key_dir_name(key, None);
            let rel = format!("pipe/{dir_name}");
            let abs = cache.root.join(&rel);
            std::fs::create_dir_all(&abs).unwrap();
            std::fs::write(abs.join("out"), content).unwrap();
            reg.entries.insert(
                rel.clone(),
                RegistryEntry {
                    pipeline: "pipe".into(),
                    key: key.into(),
                    size_bytes: content.len() as i64,
                    last_used_at_ms: ts,
                },
            );
            persist_registry(&cache.root, &reg);
        };
        mk("old", b"AAA", 100); // 3 字节
        mk("mid", b"BBB", 200);
        mk("new", b"CCC", 300); // 总 9，超 6 → 淘汰最老的 old。
        assert_eq!(cache.cache_bytes(), 9);

        // 触发淘汰：再 save 一条（0 字节内容不计，但走 evict 路径）。
        // 用 save 触发 evict：save 一条 1 字节、ts 最新的缓存。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("out"), b"D").unwrap();
        // 把 just_saved 排除后，最老的是 old(100) → 淘汰。
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "newest".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );
        // old 被淘汰（目录消失、registry 移除）。
        assert!(
            !cache.root().join("pipe").join("old").exists(),
            "最老的 old 被淘汰"
        );
        // 总量回到上限内（<= 6 + 1 = 7？淘汰 old(3) 后总 9-3+1=7 > 6 → 应再淘汰
        // mid(3) → 7-3=4 <= 6）。mid 亦应被淘汰。
        assert!(
            !cache.root().join("pipe").join("mid").exists(),
            "mid 也被淘汰回到上限内"
        );
        assert!(cache.root().join("pipe").join("new").exists(), "new 保留");
        assert!(
            cache.root().join("pipe").join("newest").exists(),
            "newest 保留"
        );
    }

    /// AC（ADR-0012「淘汰跳过被锁 key」）：对最老的缓存持共享读锁（模拟一个
    /// restore 在读）→ save 触发淘汰时跳过它，淘汰落到下一个最老的未锁 key。
    /// 真实 fs4 锁验证非阻塞 try_lock 路径。
    #[tokio::test]
    async fn eviction_skips_locked_keys() {
        let dir = tempfile::tempdir().expect("临时缓存根");
        // 容量 3 字节：三条 2 字节缓存总 6 > 3 → 需淘汰。
        let cache = Cache::new(dir.path().join("cache"), 3);
        let mk = |key: &str, content: &[u8], ts: i64| {
            let dir_name = key_dir_name(key, None);
            let rel = format!("pipe/{dir_name}");
            let abs = cache.root.join(&rel);
            std::fs::create_dir_all(&abs).unwrap();
            std::fs::write(abs.join("out"), content).unwrap();
            let mut reg = cache.registry.lock().expect("registry 锁");
            reg.entries.insert(
                rel.clone(),
                RegistryEntry {
                    pipeline: "pipe".into(),
                    key: key.into(),
                    size_bytes: content.len() as i64,
                    last_used_at_ms: ts,
                },
            );
            persist_registry(&cache.root, &reg);
        };
        // last_used 升序：old(100) 最老 → mid(200) → new(300) 最新。
        mk("old", b"AA", 100);
        mk("mid", b"BB", 200);
        mk("new", b"CC", 300);

        // 对最老的 old 持共享读锁（模拟在途 restore）。
        let old_dir = cache.root.join("pipe").join("old");
        let old_lock = open_lock(&old_dir, /*shared=*/ true).expect("锁 old");

        // save 一条 1 字节缓存触发淘汰：总 6+1=7 > 3。淘汰顺序 old(100) 先，但
        // old 被锁 → 跳过；mid(200) 次之 → 淘汰；回到上限内（剩 old+new+newest）。
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("out"), b"D").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "newest".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );

        assert!(old_dir.exists(), "被锁的 old 不被淘汰");
        assert!(
            !cache.root().join("pipe").join("mid").exists(),
            "mid 被淘汰（最老的未锁 key）"
        );
        assert!(cache.root().join("pipe").join("new").exists(), "new 保留");
        assert!(
            cache.root().join("pipe").join("newest").exists(),
            "newest 保留"
        );
        drop(old_lock);
    }

    /// AC（ADR-0012「单条超过上限直接跳过保存并告警」）：单条缓存大于容量上限
    /// → 不保存（registry 空、目录不建）。
    #[tokio::test]
    async fn save_single_over_capacity_skips() {
        let dir = tempfile::tempdir().expect("临时缓存根");
        let cache = Cache::new(dir.path().join("cache"), 4); // 容量 4 字节
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("big"), b"12345678").unwrap(); // 8 字节 > 4
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["big".into()],
                files: vec![],
            },
            ws.path(),
        );
        assert!(cache.list().is_empty(), "单条超上限不保存");
        assert_eq!(cache.cache_bytes(), 0);
    }

    /// AC（ADR-0012「单条超过上限跳过」——重叠路径不误跳）：`paths` 声明重叠
    /// （`d` 含 `d/sub`），实测大小未超上限。去重后估计不重复计数 → 不误跳、
    /// 正常保存（不去重则估计翻倍致误跳）。
    #[tokio::test]
    async fn save_overlapping_paths_not_falsely_skipped() {
        let dir = tempfile::tempdir().expect("临时缓存根");
        // 容量 4 字节；文件 3 字节（< 4）。不去重估计 = 3+3 = 6 > 4 → 误跳。
        let cache = Cache::new(dir.path().join("cache"), 4);
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::create_dir_all(ws.path().join("d").join("sub")).unwrap();
        std::fs::write(ws.path().join("d").join("sub").join("f"), b"abc").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["d".into(), "d/sub".into()],
                files: vec![],
            },
            ws.path(),
        );
        assert_eq!(
            cache.cache_bytes(),
            3,
            "重叠路径去重后不误跳，正常保存（实测 3 字节）"
        );
        // restore 往返：拷回 d/sub/f。
        let ws2 = tempfile::tempdir().expect("restore 工作区");
        cache
            .restore_blocking(
                "pipe",
                &CacheSpec {
                    key: "k".into(),
                    paths: vec!["d".into()],
                    files: vec![],
                },
                ws2.path(),
            )
            .unwrap();
        assert_eq!(
            std::fs::read(ws2.path().join("d").join("sub").join("f")).unwrap(),
            b"abc"
        );
    }

    // --- 指令（列表 / 删除）---

    #[tokio::test]
    async fn list_and_delete_via_state() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("out"), b"data").unwrap();
        cache.save_blocking(
            "pipe-a",
            &CacheSpec {
                key: "k1".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );
        cache.save_blocking(
            "pipe-b",
            &CacheSpec {
                key: "k2".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );

        // 列表：两条，真名 + 大小 + 最近使用。
        let mut entries = cache.list();
        entries.sort_by_key(|a| (a.pipeline.clone(), a.key.clone()));
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].pipeline.clone(), entries[0].key.clone()),
            ("pipe-a".into(), "k1".into())
        );
        assert_eq!(
            (entries[1].pipeline.clone(), entries[1].key.clone()),
            ("pipe-b".into(), "k2".into())
        );
        assert_eq!(entries[0].size_bytes, 4);

        // 单 key 删除（点名真完整 key）。
        cache.delete("k1");
        assert!(!cache.root().join("pipe-a").join("k1").exists(), "k1 已删");
        assert!(cache.root().join("pipe-b").join("k2").exists(), "k2 保留");
        assert_eq!(cache.list().len(), 1, "registry 同步移除 k1");

        // 全清。
        cache.delete("");
        assert_eq!(cache.list().len(), 0);
        assert!(!cache.root().join("pipe-b").join("k2").exists());
    }

    #[tokio::test]
    async fn list_drops_registry_entries_whose_dir_missing() {
        let (_dir, cache) = cache_dir();
        // 手工写一条 registry 指向不存在的目录。
        {
            let mut reg = cache.registry.lock().unwrap();
            reg.entries.insert(
                "pipe/ghost".into(),
                RegistryEntry {
                    pipeline: "pipe".into(),
                    key: "ghost".into(),
                    size_bytes: 10,
                    last_used_at_ms: 1,
                },
            );
            persist_registry(&cache.root, &reg);
        }
        let entries = cache.list();
        assert!(entries.is_empty(), "落盘核对剔除幽灵条目");
        // registry 已 persist 清理。
        let reg: Registry = serde_json::from_str(
            &std::fs::read_to_string(cache.root().join(REGISTRY_FILE)).unwrap(),
        )
        .unwrap();
        assert!(reg.entries.is_empty());
    }

    // --- handle 集成（指令 → 状态 → 上行）---

    #[tokio::test]
    async fn handle_list_sends_cache_list_uplink() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("out"), b"x").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );
        let (tx, mut rx) = mpsc::channel::<ChannelMessage>(8);
        cache.set_live(Some(tx)).await;

        cache
            .handle(sisyphus_proto::agent::CacheCommand {
                kind: Some(CacheKind::List(Default::default())),
            })
            .await;

        let msg = rx.recv().await.expect("上行列表响应");
        let list = match msg.kind {
            Some(Kind::CacheList(l)) => l,
            _ => panic!("期望 CacheList"),
        };
        assert_eq!(list.entries.len(), 1);
        assert_eq!(list.entries[0].pipeline, "pipe");
        assert_eq!(list.entries[0].key, "k");
    }

    #[tokio::test]
    async fn handle_delete_removes_dir() {
        let (_dir, cache) = cache_dir();
        let ws = tempfile::tempdir().expect("工作区");
        std::fs::write(ws.path().join("out"), b"x").unwrap();
        cache.save_blocking(
            "pipe",
            &CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            ws.path(),
        );
        let target = cache.root().join("pipe").join("k");
        assert!(target.exists());
        cache
            .handle(sisyphus_proto::agent::CacheCommand {
                kind: Some(CacheKind::Delete(CacheDeleteRequest { key: "k".into() })),
            })
            .await;
        // delete 走 spawn_blocking，轮询到删完。
        for _ in 0..50 {
            if !target.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!target.exists(), "handle 删除已删");
    }

    #[tokio::test]
    async fn handle_list_without_live_does_not_send() {
        let (_dir, cache) = cache_dir();
        // 无 set_live → send_up 仅记日志，不 panic、不阻塞。
        cache
            .handle(sisyphus_proto::agent::CacheCommand {
                kind: Some(CacheKind::List(Default::default())),
            })
            .await;
    }
}
