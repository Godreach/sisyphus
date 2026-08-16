# 0014 - 认证与用户体系：session cookie + argon2 + 项目三档角色 + 哈希落库 token

日期：2026-08-15
状态：已接受

## 背景

简单多用户体系（注册/登录 + 项目级 viewer/runner/admin 三档，ADR-0001 时已划边界：不做组织/团队 RBAC）的实现决策。已有约束：axum 0.8 REST（ADR-0005）、SQLite + sqlx（ADR-0004）、Agent 注册认证形态已定（ADR-0007：一次性注册码换长期 per-Agent token）、SPA 为 rust-embed 同源内嵌（ADR-0005）、setup wizard 创建首个管理员（CONTEXT.md 词条）。盘问过程见 [wayfinder 票 #10](https://github.com/Godreach/sisyphus/issues/10)。

## 决策

**用户与全局管理员**

- 用户表加 `is_admin` 布尔标志。全局资源（建/删项目、Agent 管理、用户管理、全局 SMTP 与注册开关配置）只认它；setup wizard 创建的第一个用户即全局 admin。
- 全局 admin **隐含全部项目的项目 admin 权限**（无需逐项目配成员；成员列表不显示，避免噪声，仍可显式分配以显式化）。
- 只禁用不物理删除：禁用即时删其全部 session、PAT，历史操作人字段永久保留、外键永不悬空。
- 注册开关进 config（默认关）：关闭时由全局 admin 建号；用户自改密码；重置密码 = 管理员代办（v1 无邮件自助重置）。

**会话与密码**

- 服务端 session + HttpOnly cookie（SameSite=Lax），session 行存 SQLite：7 天滑动过期、Server 重启不掉线、登出/禁用即删。
- 密码 argon2id（RustCrypto `argon2` crate，纯 Rust、交叉编译友好），OWASP 参数 m=19MiB/t=2/p=1；最小长度 8，无复杂度规则、无强制过期。
- CSRF：Lax 之上加集中 middleware，非安全方法（POST/PUT/DELETE）校验 Origin / Sec-Fetch-Site 同源，不匹配即拒（Bearer 天然免疫，仅管 cookie 面）。
- 登录限流：进程内内存计数（per-IP + per-username，5 次失败冷却 1 分钟、递增封顶），不持久锁定、重启即清。

**API token（两族不混用）**

- PAT：随机 32 字节 base64url、`sis_` 前缀、DB 只存 SHA-256、可选过期、UI 可吊销；`Authorization: Bearer` 提交，与 cookie 同权重放；权限 = owner 本人（v1 不做 scope 细分）。
- Agent token（ADR-0007 形态）同等对待：哈希落库、独立前缀 `sisa_`。

**权限检查层**

- 两层：认证（cookie/Bearer -> 用户，失败 401）做全局 middleware；授权（403）做每端点声明的自定义 extractor（从路径解析 project -> 查角色 -> 判档位）。
- 权限矩阵本体集中在一个 policy 模块，extractor 引用之；端点只声明（如 `Require(ProjectAdmin)`）不实现。
- 未认证可达面仅：login、setup wizard（仅用户表为空时）、register（开关限定，B2b-T4 增补）、健康检查。

**三档权限矩阵**

| 动作 | viewer | runner | 项目 admin |
|---|---|---|---|
| 查看项目/定义/构建/日志/产物列表 | ✓ | ✓ | ✓ |
| 下载产物 | ✓ | ✓ | ✓ |
| 触发/取消/重跑 | ✗ | ✓ | ✓ |
| pipeline 编辑保存（新 revision） | ✗ | ✗ | ✓ |
| 触发器/通知/项目设置 | ✗ | ✗ | ✓ |
| 成员管理（分配项目角色） | ✗ | ✗ | ✓ |
| 工作区清理、缓存手动删除 | ✗ | ✗ | ✓ |

- 用户目录可见性：项目 admin 可读最小用户目录（仅 id + username，只读，供成员分配下拉）；完整用户管理全局 admin 专属。
- 占位（细节归后续机密票）：任务机密管理归项目 admin，机密值永不可读。

## 理由

- **session 而非 JWT**：单进程 + SQLite 设计里无状态零收益；"禁用用户即刻踢线"必须有吊销能力，JWT 也得加黑名单表；rust-embed 同源内嵌下 cookie 对 SSE/EventSource 天然友好。
- **端点声明授权而非全塞中间件**：角色是「项目 × 用户」函数，统一中间件拿不到路径参数；分层路由挂载在角色×动作组合变多后比端点声明难读。集中 policy 模块保住单点审计。
- **argon2id 纯 Rust**：发布形态 6 目标交叉编译（ADR-0010），C 依赖（bcrypt/libxcrypt）是构建链风险；argon2 是 OWASP 当下首选。
- **哈希落库 + 前缀**：DB 泄露不等于 token 泄露；前缀让 secret 出现在日志/issue 时一眼可辨。两族 token 不混用防串用。
- **只禁用不删除**：CI 历史是审计对象，"这个构建谁触发的"不能因删号变 NULL；也是 Jenkins/Gitea 的主流实践。
- **内存限流不持久锁**：内网威胁模型下够用，且避免"恶意锁死他人账号"的拒绝服务面。

## 后果

- `auth` 模块（ADR-0009 已列）承载：session、PAT、policy、登录限流；middleware/extractor 挂 axum 面。
- 用户、session、PAT 三张表迁移随首个迁移批次进 SQLite。
- 每端点必须声明授权 extractor，漏声明即无权限保护（裸 401 后全放行）--CI 用 OpenAPI snapshot + code review 兜底，v1 不做静态扫描。
- 解锁下游：任务机密与审计日志票（迷雾 graduate）。
