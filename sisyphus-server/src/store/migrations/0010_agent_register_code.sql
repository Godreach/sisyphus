-- 0010 Agent 注册码一次性 + 短有效期（票 #57，Spec B3 §7 + ADR-0007/0010）
-- 注册码换 token 流程收口：register_code_used 一次性置位（兑码即 1，防重放）、
-- register_code_expires_at 短有效期（Unix 毫秒；ADR-0010：一次性 + 24h 过期）。
-- 既有行（迁移前建）expires_at 为空 = 不失效（遗留语义：注册码随建条目已签发、
-- 管理面按需处理）；新建条目随建随签 24h。注册码与 token 语义沿用 B2c 表结构
-- （register_code_hash 仅存哈希，明文只在建条目响应出现一次）。

ALTER TABLE agents ADD COLUMN register_code_used INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN register_code_expires_at INTEGER;
