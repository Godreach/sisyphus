//! 构建终态通知（票 #46 留位 → B5-T5 接通发送，ADR-0006/0014/0015）。
//!
//! [`spawn_notifier`] 订阅事件总线、过滤构建终态事件，按 pipeline 级
//! `notification` 配置（快照内 `recipients` + `on_success`）经全局 SMTP 配置
//! 发邮件：**失败终态必发、成功按 `on_success`**（ADR-0006）；收件人空或
//! SMTP 未配置则跳过并 trace；发送失败 = 告警日志不判败、不重试风暴
//! （一次终态一次发送尝试，ADR-0006）。邮件内容含构建号 / pipeline / 项目 /
//! 状态 / 触发者（验收断言）。
//!
//! **测试缝**（trait seam，issue 允许的「本地 listener 或 trait 缝注入」之二）：
//! [`MailSender`] 抽象发送，生产 [`SmtpSender`]（lettre）实现，测试侧自建
//! 假实现捕获 [`MailMessage`] 断言内容——不绑端口、不真连 SMTP。原生
//! async-in-trait + RPITIT `+ Send` bound（无 async-trait 依赖）。

use std::future::Future;

use crate::api::AppState;
use crate::engine;
use crate::events::{Event, EventBus};
use crate::secrets::{MasterKey, decrypt};
use crate::store::builds::{BuildRepo, BuildStatus};
use crate::store::smtp_config::SmtpTls;
use crate::store::smtp_config::SmtpConfigRow;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};

// ---------------------------------------------------------------------------
// 发送抽象（测试缝）
// ---------------------------------------------------------------------------

/// 一封待发邮件（from / 收件人 / 主题 / 正文）。`from` 来自全局 SMTP 配置的
/// 发件人；正文含构建号 / pipeline / 项目 / 状态 / 触发者（验收断言内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    /// 发件人（全局 SMTP 配置的 from_address）。
    pub from: String,
    /// 收件人邮箱列表（pipeline 级 notification.recipients）。
    pub to: Vec<String>,
    /// 主题。
    pub subject: String,
    /// 正文（纯文本）。
    pub body: String,
}

/// SMTP 连接参数（密码仍为密文，[`MailSender`] 实现侧解密——明文不过 trait 边界）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConnection {
    /// SMTP 主机。
    pub host: String,
    /// SMTP 端口。
    pub port: i64,
    /// 加密模式。
    pub tls: SmtpTls,
    /// SMTP AUTH 用户名（可空——无认证）。
    pub username: Option<String>,
    /// 密码的「版本字节 + nonce + 密文」形态（可空——无密码）。
    pub password_ciphertext: Option<Vec<u8>>,
}

/// 发送错误（告警日志用，不判败、不重试）。
#[derive(Debug)]
pub enum MailSendError {
    /// 密码解密失败。
    Decrypt(String),
    /// 邮件 / 传输构造失败（地址解析、TLS builder 等）。
    Build(String),
    /// SMTP 发送失败（网络 / 认证 / 投递）。
    Send(String),
}

impl std::fmt::Display for MailSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decrypt(s) => write!(f, "密码解密失败：{s}"),
            Self::Build(s) => write!(f, "邮件/传输构造失败：{s}"),
            Self::Send(s) => write!(f, "SMTP 发送失败：{s}"),
        }
    }
}

/// 发送器抽象（测试缝）。生产实现 [`SmtpSender`]（lettre）；测试侧自建假实现
/// 捕获 [`MailMessage`] 断言内容。`send` 返回 `impl Future + Send`——原生
/// async-in-trait + RPITIT，不引 async-trait。
pub trait MailSender: Send + Sync {
    /// 发一封邮件经给定连接。失败由调用侧记告警、不判败不重试（ADR-0006）。
    fn send(
        &self,
        conn: SmtpConnection,
        msg: MailMessage,
    ) -> impl Future<Output = Result<(), MailSendError>> + Send;
}

// ---------------------------------------------------------------------------
// 生产发送器：lettre
// ---------------------------------------------------------------------------

/// lettre SMTP 发送器（生产实现）。持主密钥用于解密 [`SmtpConnection`] 的密码
/// 密文——明文仅在发送瞬间存在、不出 [`MailSender::send`] 边界。
#[derive(Debug, Clone)]
pub struct SmtpSender {
    master_key: MasterKey,
}

impl SmtpSender {
    /// 由主密钥构造（main.rs 注入 `state.master_key`）。
    pub fn new(master_key: MasterKey) -> Self {
        Self { master_key }
    }
}

impl MailSender for SmtpSender {
    fn send(
        &self,
        conn: SmtpConnection,
        msg: MailMessage,
    ) -> impl Future<Output = Result<(), MailSendError>> + Send {
        // MasterKey: Copy——拷出避免 `&self` 跨 await，保 future Send。
        let key = self.master_key;
        async move {
            // 解密密码（密文 → 明文，仅发送瞬间存在）。
            let password: Option<Vec<u8>> = match conn.password_ciphertext.as_deref() {
                Some(blob) => Some(
                    decrypt(&key, blob).map_err(|e| MailSendError::Decrypt(e.to_string()))?,
                ),
                None => None,
            };

            // 按 TLS 模式构 transport builder（port 由调用侧给，覆盖默认 25/465）。
            let mut builder = match conn.tls {
                SmtpTls::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&conn.host),
                SmtpTls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(
                    &conn.host,
                )
                .map_err(|e| MailSendError::Build(e.to_string()))?,
                SmtpTls::Implicit => {
                    AsyncSmtpTransport::<Tokio1Executor>::relay(&conn.host)
                        .map_err(|e| MailSendError::Build(e.to_string()))?
                }
            };
            builder = builder.port(conn.port as u16);
            // 仅当用户名非空且有密码时启用认证（无认证 SMTP：两者皆空）。
            let creds = match (conn.username.as_deref(), password.as_deref()) {
                (Some(user), Some(pw)) if !user.is_empty() => Some(Credentials::new(
                    user.to_string(),
                    String::from_utf8_lossy(pw).into_owned(),
                )),
                _ => None,
            };
            if let Some(c) = creds {
                builder = builder.credentials(c);
            }
            let transport = builder.build();

            // 构邮件（from / 多 to / subject / body）。
            let mut email_builder = Message::builder().from(
                msg.from
                    .parse::<Mailbox>()
                    .map_err(|e| MailSendError::Build(format!("发件人地址解析失败：{e}")))?,
            );
            for to in &msg.to {
                email_builder = email_builder.to(
                    to.parse::<Mailbox>()
                        .map_err(|e| MailSendError::Build(format!("收件人地址解析失败：{e}")))?,
                );
            }
            let email = email_builder
                .subject(&msg.subject)
                .body(msg.body)
                .map_err(|e| MailSendError::Build(e.to_string()))?;

            transport
                .send(email)
                .await
                .map_err(|e| MailSendError::Send(e.to_string()))?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// 终态过滤 + 渲染
// ---------------------------------------------------------------------------

/// 是否为值得挂接通知的终态构建事件（失败必发、成功可配——成功与否由
/// 通知逻辑读快照配置裁决，这里只过滤「终态」这一事实）。
pub fn is_notifiable_terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::BuildStatus { status, .. } if status.is_terminal()
    )
}

/// 构建状态的人读中文标签（邮件正文/主题用）。
fn status_label(status: BuildStatus) -> &'static str {
    match status {
        BuildStatus::Succeeded => "成功",
        BuildStatus::Failed => "失败",
        BuildStatus::Cancelled => "已取消",
        BuildStatus::Timeout => "超时",
        BuildStatus::Running => "运行中",
        BuildStatus::Queued => "排队中",
    }
}

/// 渲染终态通知邮件。正文含构建号 / pipeline / 项目 / 状态 / 触发者
/// （验收断言内容）；主题带状态 + 项目/pipeline + 构建号速览。
fn render_mail(
    from: &str,
    recipients: &[String],
    project: &str,
    pipeline: &str,
    number: i64,
    status: BuildStatus,
    triggerer: &str,
) -> MailMessage {
    let label = status_label(status);
    let subject = format!("[sisyphus] {label}：{project}/{pipeline} #{number}");
    let body = format!(
        "项目：{project}\n\
         Pipeline：{pipeline}\n\
         构建号：#{number}\n\
         状态：{label}\n\
         触发者：{triggerer}\n"
    );
    MailMessage {
        from: from.to_string(),
        to: recipients.to_vec(),
        subject,
        body,
    }
}

// ---------------------------------------------------------------------------
// 通知后台任务
// ---------------------------------------------------------------------------

/// 订阅事件总线、按终态构建的 pipeline 级 `notification` 配置经全局 SMTP 配置
/// 发邮件的后台任务。失败终态必发、成功按 `on_success`（ADR-0006）；收件人空 /
/// SMTP 未配置 → 跳过并 trace；发送失败 = 告警日志不判败、不重试（一次终态
/// 一次发送尝试）。`sender` 注入便于测试缝（生产传 [`SmtpSender`]）。
pub fn spawn_notifier<S: MailSender + 'static>(
    bus: EventBus,
    state: AppState,
    sender: S,
) -> tokio::task::JoinHandle<()> {
    // BuildRepo 供读构建快照（notification 配置）+ trigger_detail（触发者）；
    // engine 未暴露 get，此处自持一份共池 repo（与 engine 同池，零额外连接）。
    let builds = BuildRepo::new(state.pool.clone());
    // 订阅先于 spawn——广播通道只投递订阅后的事件，spawn 内订阅会丢「订阅前
    // 已发」事件（启动竞态）。这里同步订阅，调用侧 spawn_notifier 后再发事件
    // 即不漏（容量 64 缓冲到 spawned 任务消费前）。
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if is_notifiable_terminal(&event) => {
                    let Event::BuildStatus {
                        build_id,
                        project_name,
                        pipeline_name,
                        number,
                        status,
                        ..
                    } = event
                    else {
                        unreachable!("is_notifiable_terminal 已过滤非 BuildStatus");
                    };
                    // 读构建行：snapshot（notification 配置）+ trigger_detail（触发者）。
                    let build = match builds.get(build_id).await {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            tracing::trace!(build_id, "终态构建行已无（通知跳过）");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                build_id,
                                error = %e,
                                "读构建行失败（通知跳过，不判败）"
                            );
                            continue;
                        }
                    };
                    let snapshot = match serde_json::from_str::<
                        sisyphus_model::validate::BuildSnapshot,
                    >(&build.snapshot)
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                build_id,
                                error = %e,
                                "解析构建快照失败（通知跳过，不判败）"
                            );
                            continue;
                        }
                    };
                    let notification = match &snapshot.pipeline.notification {
                        Some(n) => n,
                        None => {
                            tracing::trace!(build_id, "无 notification 配置（跳过）");
                            continue;
                        }
                    };
                    if notification.recipients.is_empty() {
                        tracing::trace!(build_id, "notification recipients 为空（跳过）");
                        continue;
                    }
                    // 失败终态必发、成功按 on_success（ADR-0006）。
                    if matches!(status, BuildStatus::Succeeded) && !notification.on_success {
                        tracing::trace!(build_id, "成功但 on_success=false（跳过）");
                        continue;
                    }
                    let triggerer = engine::trigger_by(&build.trigger_detail);

                    // 读全局 SMTP 配置（单次）：未配置 → 跳过并 trace。
                    let smtp = match state.smtp_config.get().await {
                        Ok(Some(r)) => r,
                        Ok(None) => {
                            tracing::trace!(build_id, "SMTP 未配置（通知跳过）");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                build_id,
                                error = %e,
                                "读 SMTP 配置失败（通知跳过，不判败）"
                            );
                            continue;
                        }
                    };
                    let conn = SmtpConnection::from_row(&smtp);
                    let msg = render_mail(
                        &smtp.from_address,
                        &notification.recipients,
                        &project_name,
                        &pipeline_name,
                        number,
                        status,
                        &triggerer,
                    );
                    match sender.send(conn, msg).await {
                        Ok(()) => tracing::info!(
                            build_id,
                            number,
                            status = status.as_str(),
                            "构建终态通知已发送"
                        ),
                        Err(e) => tracing::warn!(
                            build_id,
                            number,
                            error = %e,
                            "通知发送失败（不判败、不重试，ADR-0006）"
                        ),
                    }
                }
                // 可丢热通知：Lagged/Closed 直接忽略、继续收。
                Ok(_) | Err(_) => continue,
            }
        }
    })
}

impl SmtpConnection {
    /// 由 repo 行转连接参数（丢弃非连接字段 from_address —— 那进 [`MailMessage`]）。
    fn from_row(row: &SmtpConfigRow) -> Self {
        Self {
            host: row.host.clone(),
            port: row.port,
            tls: row.tls,
            username: row.username.clone(),
            password_ciphertext: row.password_ciphertext.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::store::builds::BuildStatus;

    fn terminal_event(status: BuildStatus) -> Event {
        Event::BuildStatus {
            build_id: 1,
            project_name: "demo".into(),
            pipeline_name: "release".into(),
            number: 1,
            status,
            attempt: 1,
        }
    }

    #[test]
    fn terminal_filter_keeps_only_build_terminal_status() {
        for status in [
            BuildStatus::Succeeded,
            BuildStatus::Failed,
            BuildStatus::Cancelled,
            BuildStatus::Timeout,
        ] {
            assert!(
                is_notifiable_terminal(&terminal_event(status)),
                "{status:?} 为终态"
            );
        }
        assert!(!is_notifiable_terminal(&terminal_event(BuildStatus::Running)));
        assert!(!is_notifiable_terminal(&terminal_event(BuildStatus::Queued)));
        assert!(!is_notifiable_terminal(&Event::JobStatus {
            job_id: 1,
            build_id: 1,
            stage_index: 0,
            name: "compile".into(),
            status: crate::store::jobs::JobStatus::Succeeded,
            attempt: 1,
        }));
    }

    #[test]
    fn render_mail_body_contains_build_number_pipeline_status_triggerer() {
        let msg = render_mail(
            "ci@example.com",
            &["dev@example.com".into()],
            "demo",
            "release",
            42,
            BuildStatus::Failed,
            "alice",
        );
        // 验收：邮件内容含构建号 / pipeline / 状态 / 触发者。
        assert!(msg.body.contains("#42"), "含构建号：{}", msg.body);
        assert!(msg.body.contains("release"), "含 pipeline：{}", msg.body);
        assert!(msg.body.contains("demo"), "含项目：{}", msg.body);
        assert!(msg.body.contains("失败"), "含状态：{}", msg.body);
        assert!(msg.body.contains("alice"), "含触发者：{}", msg.body);
        assert_eq!(msg.from, "ci@example.com");
        assert_eq!(msg.to, vec!["dev@example.com".to_string()]);
        assert!(msg.subject.contains("release"), "主题含 pipeline");
        assert!(msg.subject.contains("#42"), "主题含构建号");
    }

    #[test]
    fn status_label_covers_all_variants() {
        assert_eq!(status_label(BuildStatus::Succeeded), "成功");
        assert_eq!(status_label(BuildStatus::Failed), "失败");
        assert_eq!(status_label(BuildStatus::Cancelled), "已取消");
        assert_eq!(status_label(BuildStatus::Timeout), "超时");
        assert_eq!(status_label(BuildStatus::Running), "运行中");
        assert_eq!(status_label(BuildStatus::Queued), "排队中");
    }
}
