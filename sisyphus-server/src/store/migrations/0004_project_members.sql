-- 0004 项目成员表（B2b-T5，ADR-0014）
-- 项目 × 用户 → 三档角色（viewer/runner/admin）。角色即档位，权限矩阵
-- 本体集中在上层 policy 模块，schema 只约束取值域。全局 admin 不落本表
-- ——隐含全部项目的项目 admin（无需逐项目配成员，ADR-0014）。
CREATE TABLE project_members (
    project_id INTEGER NOT NULL REFERENCES projects(id),
    user_id    INTEGER NOT NULL REFERENCES users(id),
    role       TEXT NOT NULL CHECK (role IN ('viewer', 'runner', 'admin')),
    PRIMARY KEY (project_id, user_id)
);

-- 用户维度索引：项目可见性过滤（list 只列有角色的项目）与用户目录守卫
-- （任一项目的 admin 即可读目录）都按 user_id 查。
CREATE INDEX idx_project_members_user ON project_members(user_id);
