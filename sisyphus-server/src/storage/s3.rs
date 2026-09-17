//! S3 兼容客户端（票 #122，ADR-0026）：SigV4 头签名 + 最低契约操作。
//! 凭据只用于签名，不写日志、不进错误正文。

use std::time::Duration;

use sha2::{Digest, Sha256};

use super::keys::probe_key;
use super::{ConnectionCheck, ConnectionTestReport, StorageError};
use crate::config::S3Config;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const PROBE_BODY: &[u8] = b"sisyphus-s3-probe-bytes";

/// 运行中的 S3 客户端（持长期凭据，仅 Server 使用）。
#[derive(Clone)]
pub struct S3Client {
    http: reqwest::Client,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    access_key_id: String,
    secret_access_key: String,
    path_style: bool,
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &"***")
            .field("secret_access_key", &"***")
            .field("path_style", &self.path_style)
            .finish()
    }
}

struct SignedRequest {
    url: String,
    headers: Vec<(String, String)>,
}

struct S3HttpResponse {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl S3Client {
    /// 由合并后的配置构造。CA 文件在配置层已校验存在。
    pub fn new(cfg: &S3Config) -> Result<Self, StorageError> {
        let mut builder = reqwest::Client::builder()
            .tls_backend_rustls()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30));
        if let Some(path) = &cfg.ca_path {
            let pem = std::fs::read(path)
                .map_err(|e| StorageError::Config(format!("读取 S3 CA 失败：{}", e)))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .map_err(|e| StorageError::Config(format!("解析 S3 CA 失败：{e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| StorageError::Config(format!("构造 S3 HTTP 客户端失败：{e}")))?;
        Ok(Self {
            http,
            endpoint: cfg.endpoint.clone(),
            region: cfg.region.clone(),
            bucket: cfg.bucket.clone(),
            prefix: cfg.prefix.clone(),
            access_key_id: cfg.access_key_id.clone(),
            secret_access_key: cfg.secret_access_key.clone(),
            path_style: cfg.path_style,
        })
    }

    /// 配置身份的非机密摘要（普通 API 回显用）。
    pub fn public_view(&self) -> S3PublicView {
        S3PublicView {
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            path_style: self.path_style,
        }
    }

    /// 根前缀。
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// 非破坏性启动校验：HEAD bucket（不写对象）。
    pub async fn head_bucket(&self) -> Result<(), StorageError> {
        let resp = self.send("HEAD", None, &[], &[], b"").await?;
        if resp.status == 200 || resp.status == 204 {
            return Ok(());
        }
        Err(map_s3_status(resp.status, &resp.body, "校验 bucket"))
    }

    /// 管理员显式连接测试：PUT/HEAD/Range GET/复制/multipart 后清理。
    pub async fn test_connection(&self) -> ConnectionTestReport {
        let probe_id = probe_id();
        let blob = probe_key(&self.prefix, &probe_id, "blob");
        let copied = probe_key(&self.prefix, &probe_id, "copy");
        let multipart = probe_key(&self.prefix, &probe_id, "mp");
        let mp_copy = probe_key(&self.prefix, &probe_id, "mp-copy");
        let mut checks = Vec::new();
        let mut upload_id: Option<String> = None;
        let mut copy_upload_id: Option<String> = None;

        match self.put_object(&blob, PROBE_BODY).await {
            Ok(()) => checks.push(ok("put")),
            Err(e) => {
                checks.push(fail("put", &e));
                self.cleanup_probe(
                    &[&blob, &copied, &multipart, &mp_copy],
                    upload_id,
                    copy_upload_id,
                )
                .await;
                return ConnectionTestReport { ok: false, checks };
            }
        }

        push_check(
            &mut checks,
            "head",
            self.head_object(&blob).await.map(|_| ()),
        );
        push_check(
            &mut checks,
            "range_get",
            self.get_range(&blob, 0, 3).await.and_then(|bytes| {
                if bytes == b"sisy" {
                    Ok(())
                } else {
                    Err(StorageError::Protocol("Range GET 内容不符合探针".into()))
                }
            }),
        );
        push_check(&mut checks, "copy", self.copy_object(&blob, &copied).await);

        match self.create_multipart(&multipart).await {
            Ok(id) => {
                upload_id = Some(id.clone());
                match self.upload_part(&multipart, &id, 1, PROBE_BODY).await {
                    Ok(etag) => {
                        push_check(
                            &mut checks,
                            "multipart_upload",
                            self.complete_multipart(&multipart, &id, &[(1, etag)]).await,
                        );
                        upload_id = None;
                    }
                    Err(e) => checks.push(fail("multipart_upload", &e)),
                }
            }
            Err(e) => checks.push(fail("multipart_upload", &e)),
        }

        match self.create_multipart(&mp_copy).await {
            Ok(id) => {
                copy_upload_id = Some(id.clone());
                match self.upload_part_copy(&mp_copy, &id, 1, &blob).await {
                    Ok(etag) => {
                        push_check(
                            &mut checks,
                            "multipart_copy",
                            self.complete_multipart(&mp_copy, &id, &[(1, etag)]).await,
                        );
                        copy_upload_id = None;
                    }
                    Err(e) => checks.push(fail("multipart_copy", &e)),
                }
            }
            Err(e) => checks.push(fail("multipart_copy", &e)),
        }

        let delete_ok = self
            .cleanup_probe(
                &[&blob, &copied, &multipart, &mp_copy],
                upload_id,
                copy_upload_id,
            )
            .await;
        if delete_ok {
            checks.push(ok("delete"));
        } else {
            checks.push(ConnectionCheck {
                op: "delete".into(),
                ok: false,
                detail: Some("探针对象清理未完全成功".into()),
            });
        }

        ConnectionTestReport {
            ok: checks.iter().all(|c| c.ok),
            checks,
        }
    }

    async fn put_object(&self, key: &str, body: &[u8]) -> Result<(), StorageError> {
        let resp = self.send("PUT", Some(key), &[], &[], body).await?;
        if (200..300).contains(&resp.status) {
            Ok(())
        } else {
            Err(map_s3_status(resp.status, &resp.body, "PUT"))
        }
    }

    async fn head_object(&self, key: &str) -> Result<u64, StorageError> {
        let resp = self.send("HEAD", Some(key), &[], &[], b"").await?;
        if !(200..300).contains(&resp.status) {
            return Err(map_s3_status(resp.status, &resp.body, "HEAD"));
        }
        Ok(resp
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    async fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, StorageError> {
        let range = format!("bytes={start}-{end}");
        let resp = self
            .send("GET", Some(key), &[], &[("Range", &range)], b"")
            .await?;
        if resp.status == 206 || resp.status == 200 {
            Ok(resp.body)
        } else {
            Err(map_s3_status(resp.status, &resp.body, "Range GET"))
        }
    }

    async fn delete_object(&self, key: &str) -> Result<(), StorageError> {
        let resp = self.send("DELETE", Some(key), &[], &[], b"").await?;
        if (200..300).contains(&resp.status) || resp.status == 404 {
            Ok(())
        } else {
            Err(map_s3_status(resp.status, &resp.body, "DELETE"))
        }
    }

    async fn copy_object(&self, src: &str, dst: &str) -> Result<(), StorageError> {
        let source = format!("/{}/{}", self.bucket, src);
        let resp = self
            .send(
                "PUT",
                Some(dst),
                &[],
                &[("x-amz-copy-source", &source)],
                b"",
            )
            .await?;
        if (200..300).contains(&resp.status) {
            Ok(())
        } else {
            Err(map_s3_status(resp.status, &resp.body, "CopyObject"))
        }
    }

    async fn create_multipart(&self, key: &str) -> Result<String, StorageError> {
        let resp = self
            .send("POST", Some(key), &[("uploads", "")], &[], b"")
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(map_s3_status(resp.status, &resp.body, "CreateMultipart"));
        }
        let body = String::from_utf8_lossy(&resp.body);
        xml_tag(&body, "UploadId")
            .ok_or_else(|| StorageError::Protocol("CreateMultipart 响应缺少 UploadId".into()))
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part: i32,
        body: &[u8],
    ) -> Result<String, StorageError> {
        let part_s = part.to_string();
        let resp = self
            .send(
                "PUT",
                Some(key),
                &[("partNumber", &part_s), ("uploadId", upload_id)],
                &[],
                body,
            )
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(map_s3_status(resp.status, &resp.body, "UploadPart"));
        }
        etag_of(&resp.headers)
            .ok_or_else(|| StorageError::Protocol("UploadPart 响应缺少 ETag".into()))
    }

    async fn upload_part_copy(
        &self,
        key: &str,
        upload_id: &str,
        part: i32,
        src: &str,
    ) -> Result<String, StorageError> {
        let part_s = part.to_string();
        let source = format!("/{}/{}", self.bucket, src);
        let resp = self
            .send(
                "PUT",
                Some(key),
                &[("partNumber", &part_s), ("uploadId", upload_id)],
                &[("x-amz-copy-source", &source)],
                b"",
            )
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(map_s3_status(resp.status, &resp.body, "UploadPartCopy"));
        }
        let body = String::from_utf8_lossy(&resp.body);
        xml_tag(&body, "ETag")
            .or_else(|| etag_of(&resp.headers))
            .ok_or_else(|| StorageError::Protocol("UploadPartCopy 响应缺少 ETag".into()))
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[(i32, String)],
    ) -> Result<(), StorageError> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (n, etag) in parts {
            xml.push_str(&format!(
                "<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>",
                xml_escape(etag)
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let resp = self
            .send(
                "POST",
                Some(key),
                &[("uploadId", upload_id)],
                &[],
                xml.as_bytes(),
            )
            .await?;
        if (200..300).contains(&resp.status) {
            Ok(())
        } else {
            Err(map_s3_status(resp.status, &resp.body, "CompleteMultipart"))
        }
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), StorageError> {
        let resp = self
            .send("DELETE", Some(key), &[("uploadId", upload_id)], &[], b"")
            .await?;
        if (200..300).contains(&resp.status) || resp.status == 404 {
            Ok(())
        } else {
            Err(map_s3_status(resp.status, &resp.body, "AbortMultipart"))
        }
    }

    async fn cleanup_probe(
        &self,
        keys: &[&str],
        upload_id: Option<String>,
        copy_upload_id: Option<String>,
    ) -> bool {
        let mut ok = true;
        if let Some(id) = upload_id
            && self.abort_multipart(keys[2], &id).await.is_err()
        {
            ok = false;
        }
        if let Some(id) = copy_upload_id
            && self.abort_multipart(keys[3], &id).await.is_err()
        {
            ok = false;
        }
        for key in keys {
            if self.delete_object(key).await.is_err() {
                ok = false;
            }
        }
        ok
    }

    async fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<S3HttpResponse, StorageError> {
        let signed = self.sign(method, key, query, extra_headers, body)?;
        let mut req = self.http.request(
            method
                .parse::<reqwest::Method>()
                .map_err(|e| StorageError::Protocol(format!("HTTP 方法非法：{e}")))?,
            &signed.url,
        );
        for (name, value) in &signed.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }
        let resp = req.send().await.map_err(StorageError::from_reqwest)?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .bytes()
            .await
            .map_err(StorageError::from_reqwest)?
            .to_vec();
        Ok(S3HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn sign(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<SignedRequest, StorageError> {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let payload_hash = if body.is_empty() {
            EMPTY_SHA256.to_string()
        } else {
            hex_encode(&Sha256::digest(body))
        };
        let (url, canonical_uri, host) = self.endpoint_parts(key);
        let canonical_query = canonical_query(query);
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), host),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        for (name, value) in extra_headers {
            headers.push((name.to_ascii_lowercase(), (*value).to_string()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        headers.dedup_by(|a, b| a.0 == b.0);

        let signed_header_names: Vec<String> = headers.iter().map(|(n, _)| n.clone()).collect();
        let canonical_headers = headers
            .iter()
            .map(|(n, v)| format!("{n}:{}\n", v.trim()))
            .collect::<String>();
        let signed_headers = signed_header_names.join(";");
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex_encode(&Sha256::digest(canonical_request.as_bytes()))
        );
        let signing_key = signing_key(&self.secret_access_key, &date_stamp, &self.region)?;
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes())?);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key_id
        );
        let mut out_headers = headers;
        out_headers.push(("authorization".into(), authorization));
        let url = if canonical_query.is_empty() {
            url
        } else {
            format!("{url}?{canonical_query}")
        };
        Ok(SignedRequest {
            url,
            headers: out_headers,
        })
    }

    fn endpoint_parts(&self, key: Option<&str>) -> (String, String, String) {
        let host = host_of(&self.endpoint);
        if self.path_style {
            let mut uri = format!("/{}", self.bucket);
            if let Some(key) = key {
                uri.push('/');
                uri.push_str(&uri_encode_path(key));
            }
            let url = format!("{}{}", self.endpoint, uri);
            (url, uri, host)
        } else {
            let host_name = host.split(':').next().unwrap_or(&host);
            let vhost = if let Some(port) = host.split_once(':').map(|(_, p)| p) {
                format!("{}.{host_name}:{port}", self.bucket)
            } else {
                format!("{}.{host_name}", self.bucket)
            };
            let scheme_end = self.endpoint.find("://").map(|i| i + 3).unwrap_or(0);
            let scheme = &self.endpoint[..scheme_end];
            let mut uri = "/".to_string();
            if let Some(key) = key {
                uri.push_str(&uri_encode_path(key));
            }
            let url = format!("{scheme}{vhost}{uri}");
            (url, uri, vhost)
        }
    }
}

/// 普通 API 可见的 S3 配置（无凭据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3PublicView {
    /// Endpoint。
    pub endpoint: String,
    /// Region。
    pub region: String,
    /// Bucket。
    pub bucket: String,
    /// 根前缀。
    pub prefix: String,
    /// path-style。
    pub path_style: bool,
}

fn ok(op: &str) -> ConnectionCheck {
    ConnectionCheck {
        op: op.into(),
        ok: true,
        detail: None,
    }
}

fn fail(op: &str, err: &StorageError) -> ConnectionCheck {
    ConnectionCheck {
        op: op.into(),
        ok: false,
        detail: Some(err.to_string()),
    }
}

fn push_check(checks: &mut Vec<ConnectionCheck>, op: &str, result: Result<(), StorageError>) {
    match result {
        Ok(()) => checks.push(ok(op)),
        Err(e) => checks.push(fail(op, &e)),
    }
}

fn probe_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms:x}-{seq}")
}

fn map_s3_status(status: u16, body: &[u8], op: &str) -> StorageError {
    let text = String::from_utf8_lossy(body);
    let code = xml_tag(&text, "Code").unwrap_or_default();
    if status == 403
        || code == "InvalidAccessKeyId"
        || code == "SignatureDoesNotMatch"
        || code == "AccessDenied"
    {
        return StorageError::Credentials(format!("{op} 凭据被拒绝"));
    }
    if status == 404 || code == "NoSuchBucket" || code == "NotFound" {
        return StorageError::MissingBucket(format!("{op} 未找到 bucket"));
    }
    StorageError::S3 {
        status,
        code,
        message: xml_tag(&text, "Message").unwrap_or_else(|| op.into()),
    }
}

fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let i = body.find(&start)?;
    let rest = &body[i + start.len()..];
    let j = rest.find(&end)?;
    Some(rest[..j].trim().trim_matches('"').to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn etag_of(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().trim_matches('"').to_string())
}

fn host_of(endpoint: &str) -> String {
    endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .trim_end_matches('/')
        .to_string()
}

fn uri_encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| aws_encode(seg, true))
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut parts: Vec<String> = query
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                format!("{}=", aws_encode(k, true))
            } else {
                format!("{}={}", aws_encode(k, true), aws_encode(v, true))
            }
        })
        .collect();
    parts.sort();
    parts.join("&")
}

fn aws_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, StorageError> {
    // AWS SigV4 HMAC-SHA256。用已有 sha2，避免再引入与 workspace 不匹配的 hmac 版本。
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = Sha256::digest(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    Ok(outer.finalize().to_vec())
}

fn signing_key(secret: &str, date: &str, region: &str) -> Result<Vec<u8>, StorageError> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, b"s3")?;
    hmac_sha256(&k_service, b"aws4_request")
}
