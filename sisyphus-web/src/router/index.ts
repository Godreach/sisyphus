// 路由表（ADR-0020 12 页 IA，票 B4-T1 骨架）。
//
// 本票立骨架与守卫语义；页面按页面票落地：概览/项目列表/项目详情（B4-T3）、
// 构建列表/构建详情（B4-T4）、Agent 列表/详情（B4-T5）、管理四页（B4-T6）
// 均已实现；pipeline 编辑仍挂 PlaceholderView（B4-T7）。路由守卫保证「未认证
// 访问受保护页 → 登录 → 登录成功回跳」闭环 + 管理区全局 admin 门控
// （`meta.admin`，守卫逻辑见 `src/router/guards.ts`）。

import { createRouter, createWebHistory } from 'vue-router'
import type { RouteRecordRaw } from 'vue-router'

import { sessionGuard } from './guards'

const routes: RouteRecordRaw[] = [
  {
    path: '/login',
    name: 'login',
    component: () => import('@/views/LoginView.vue'),
    meta: { public: true },
  },
  {
    // 初始化引导位：空库时进入（`/auth/setup` 空库 404 判定，B4-T2 细化）。
    path: '/setup',
    name: 'setup',
    component: () => import('@/views/SetupView.vue'),
    meta: { public: true },
  },
  {
    path: '/',
    name: 'overview',
    component: () => import('@/views/OverviewView.vue'),
    meta: { title: 'routes.overview' },
  },
  {
    path: '/projects',
    name: 'projects',
    component: () => import('@/views/ProjectsView.vue'),
    meta: { title: 'routes.projects' },
  },
  {
    path: '/projects/:name',
    name: 'project-detail',
    component: () => import('@/views/ProjectDetailView.vue'),
    meta: { title: 'routes.projectDetail' },
  },
  {
    path: '/projects/:name/pipelines/:pipeline',
    name: 'pipeline-edit',
    component: () => import('@/views/PlaceholderView.vue'),
    meta: { title: 'routes.pipelineEdit' },
  },
  {
    path: '/projects/:name/pipelines/:pipeline/builds',
    name: 'build-list',
    component: () => import('@/views/BuildListView.vue'),
    meta: { title: 'routes.buildList' },
  },
  {
    path: '/projects/:name/pipelines/:pipeline/builds/:number',
    name: 'build-detail',
    component: () => import('@/views/BuildDetailView.vue'),
    meta: { title: 'routes.buildDetail' },
  },
  {
    path: '/agents',
    name: 'agents',
    component: () => import('@/views/AgentListView.vue'),
    meta: { title: 'routes.agents' },
  },
  {
    path: '/agents/:name',
    name: 'agent-detail',
    component: () => import('@/views/AgentDetailView.vue'),
    meta: { title: 'routes.agentDetail' },
  },
  {
    path: '/admin/secrets',
    name: 'admin-secrets',
    component: () => import('@/views/SecretsView.vue'),
    meta: { title: 'routes.adminSecrets', admin: true },
  },
  {
    path: '/admin/audit',
    name: 'admin-audit',
    component: () => import('@/views/AuditView.vue'),
    meta: { title: 'routes.adminAudit', admin: true },
  },
  {
    path: '/admin/upgrade',
    name: 'admin-upgrade',
    component: () => import('@/views/AgentUpgradeView.vue'),
    meta: { title: 'routes.adminUpgrade', admin: true },
  },
  {
    path: '/admin/users',
    name: 'admin-users',
    component: () => import('@/views/UsersView.vue'),
    meta: { title: 'routes.adminUsers', admin: true },
  },
  {
    path: '/:pathMatch(.*)*',
    name: 'not-found',
    component: () => import('@/views/NotFoundView.vue'),
    meta: { public: true },
  },
]

export const router = createRouter({
  history: createWebHistory(),
  routes,
})

// 守卫：会话恢复 + 未认证重定向登录（guards.ts）。
router.beforeEach(sessionGuard)

export default router
