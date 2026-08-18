//! 静态资源服务（ADR-0005；票 B2a-T5）。
//!
//! rust-embed 内嵌 sisyphus-web 构建产物（票 B4-T1 起为真实 Vue 构建）：
//! release 编译期嵌入、debug 运行时读盘——构建产物放进
//! `sisyphus-web/dist/` 后 server 侧零改动。
//!
//! 非 `/api` 路径的解析顺序（票 B2a-T5 AC）：**本地覆盖目录 → 内嵌资源 →
//! SPA fallback 回 index.html**。同名文件覆盖目录压过内嵌（ADR-0005 引
//! Gitea 的分层资产模式：数据目录 `web/` 子目录放同名文件即覆盖，目录
//! 不存在即无覆盖层）；index.html 同样遵循该优先级。

use std::path::{Path, PathBuf};

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use rust_embed::RustEmbed;

/// 内嵌的前端产物（sisyphus-web/dist，路径相对本 crate 根解析）。
#[derive(RustEmbed)]
#[folder = "../sisyphus-web/dist/"]
struct WebDist;

/// 非 `/api` 未命中路径的静态解析入口（由 [`crate::api`] 根层 fallback 调入）。
pub(crate) fn serve(override_dir: &Path, uri_path: &str) -> Response {
    let rel = sanitize_rel_path(uri_path);

    // 1. 本地覆盖目录：同名文件优先于内嵌（含嵌套资产）。
    if let Some(resp) = rel.as_deref().and_then(|r| serve_override(override_dir, r)) {
        return resp;
    }

    // 2. 内嵌资源。
    if let Some(rel_str) = rel.as_deref().and_then(Path::to_str)
        && let Some(file) = WebDist::get(rel_str)
    {
        return file_response(Path::new(rel_str), file.data.into_owned());
    }

    // 3. SPA fallback：回 index.html（覆盖目录同名文件同样优先）。
    serve_index(override_dir)
}

/// 覆盖目录层：文件存在即应答——读取失败按 500 fail-loud（操作者显式放置
/// 的文件不得被静默换成内嵌旧版）；不存在返回 [`None`] 交下一层。
fn serve_override(override_dir: &Path, rel: &Path) -> Option<Response> {
    let candidate = override_dir.join(rel);
    if !candidate.is_file() {
        return None;
    }
    match std::fs::read(&candidate) {
        Ok(bytes) => Some(file_response(&candidate, bytes)),
        Err(e) => {
            tracing::warn!(path = %candidate.display(), error = %e, "覆盖文件读取失败");
            Some(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// SPA 入口页：覆盖目录 index.html 优先，回落内嵌；两层皆缺（内嵌产物
/// 未随构建产出）才 404——正常提交里入口页必在。
fn serve_index(override_dir: &Path) -> Response {
    if let Some(resp) = serve_override(override_dir, Path::new("index.html")) {
        return resp;
    }
    match WebDist::get("index.html") {
        Some(file) => file_response(Path::new("index.html"), file.data.into_owned()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 以字节数组 + 按扩展名的 Content-Type 应答（Content-Length 由 axum 补全）。
fn file_response(path: &Path, bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, mime_for(path))], bytes).into_response()
}

/// URL 路径 → 覆盖目录/内嵌资源的相对路径。百分号解码后逐段拼接；任何
/// 穿越形态（`..` 段、段内反斜杠或盘符冒号——Windows 分隔符语义）与非
/// UTF-8 解码失败都返回 [`None`]，由调用方落 SPA 入口页，不外泄文件系统。
/// 根路径（空相对路径）同样返回 [`None`]：它本来就是 index.html。
fn sanitize_rel_path(uri_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode_str(uri_path).decode_utf8().ok()?;
    let mut rel = PathBuf::new();
    for seg in decoded.trim_start_matches('/').split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None,
            s if s.contains(['\\', ':']) => return None,
            s => rel.push(s),
        }
    }
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel)
    }
}

/// 常见 Web 扩展的 Content-Type 表（「真实前端产物接入零改动」的配套：
/// 构建产物常见类型在此一次列全），未知扩展回落 application/octet-stream。
fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map" | "webmanifest") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        _ => "application/octet-stream",
    }
}
