# 0022 - SMTP 传输 TLS 经 ring（对 ADR-0015「避 C 加密」的例外修正）

日期：2026-08-20
状态：已接受

## 背景

ADR-0015「密钥文件 + XChaCha20-Poly1305」的「理由」段把机密值加密选型定在纯 Rust（chacha20poly1305，无 AES 硬件依赖）以契合 ADR-0010 的 6 目标交叉编译；同一理由随后被引为「仓库避 C 加密立场」。

B5-T5（票 #77）接通构建终态邮件通知，需一个 SMTP 客户端 crate（候选 lettre）。SMTP 传输的 TLS 加密不存在纯 Rust 实现：lettre 的 `rustls-tls` 后端经 rustls 加密提供方 `ring`，后者含 C/asm（预编译汇编 + `cc` 编译期 glue）；唯一替代是 `native-tls`/openssl（更糟——OS 系统库，交叉编译更难）。`default-features=false` 已排除 native-tls/openssl（`cargo tree` 审计无二者），故 TLS 只剩 ring。

CONTRIBUTING.md「架构变更」段要求：涉及架构取舍的改动先起 ADR。本 ADR 即对 ADR-0015「纯 Rust 无 C 依赖」理由的修正记录。

## 决策

**接受 ring 作为 SMTP 传输 TLS 的加密提供方**，并将其作为 ADR-0015「避 C 加密」立场的**有界例外**——边界明确：

- **机密值加密**（DB 落库的 SMTP 密码、任务机密、SCM 凭据）**仍走纯 Rust chacha20poly1305**，ADR-0015 不变。ring 仅用于 **SMTP 传输 TLS**（lettre → rustls → ring），值不进 ring 路径。
- **lettre `default-features=false` + `tokio1-rustls-tls`**：只引 rustls 这一条 TLS 路径，排除 native-tls/openssl。
- **ring 依赖现状**：原生支持 6 目标（x86_64/aarch64 × linux/windows/macos），`cargo tree` 经 rustls 间接拉入；本地 native 构建（Windows x86_64）已验绿。

## 理由

- **纯 Rust TLS 不存在**：这是 crate 生态的客观约束，非选型偏好。ring 是该约束下交叉编译最友好的唯一选项——openssl/native-tls 依赖 OS 系统库，6 目标矩阵更难。
- **威胁模型不退步**：SMTP 密码在 DB 仍由 chacha20poly1305 加密落库（ADR-0015 防护边界：DB 文件单独泄露不暴露密码）；ring 只管传输层加密，与「DB 备份单独泄露」这一 ADR-0015 主防护目标无关。
- **明确边界而非泛化**：把例外限定在「SMTP 传输 TLS」一处，不开放「其它处亦可引 C」的口子；机密值加密纪律不变。

## 后果

- ADR-0015「理由」段的「纯 Rust 无 C 依赖」修正为「机密值加密纯 Rust（chacha20poly1305）；SMTP 传输 TLS 经 ring（C/asm），无纯 Rust 替代」。
- 6 目标交叉编译矩阵（ADR-0010）现额外依赖 ring 的 C 工具链——与既有 libsqlite3-sys（sqlx）同族 C 依赖并存。本地 Windows→Linux 交叉编译被该 C 工具链需求阻断（非 lettre 特有），6 目标矩阵的**原生**构建（每个目标在其原生 OS 上 `cargo build`）由 B5-T10/CI 验证，不在本票验。
- 依赖树增大：rustls/ring 等随 lettre 进树，发布二进制体积略增（SMTP 通知是可选运维能力，体积代价可接受）。
- v2 候选：若出现纯 Rust TLS（如 aws-lc-rs 的纯 Rust 路径成熟、或 rustls 的 RustCrypto provider 默认化），可退回无 C 依赖。
