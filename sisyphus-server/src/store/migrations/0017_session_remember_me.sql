-- 0017 会话 remember_me（票 #114，ADR-0014）
-- 登录勾选「保持登录」时会话行落 remember_me=1：cookie 带 30 天 Max-Age（持久，
-- 关浏览器再开仍登录）+ 服务端 expires_at 按 30 天滑动；缺省/未勾选 remember_me=0：
-- 会话级 cookie（无 Max-Age，关浏览器即失效）+ 服务端仍按 7 天滑动过期（过期清理面）。
-- 认证中间件按本字段决定滑动 TTL 与续发 cookie 形态（Max-Age 有无），故需落库
-- 跨请求持久（中间件只凭 session 行还原会话形态，不读 cookie 属性）。
ALTER TABLE sessions ADD COLUMN remember_me INTEGER NOT NULL DEFAULT 0 CHECK (remember_me IN (0, 1));
