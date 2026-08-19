<script setup lang="ts">
// 应用壳（ADR-0020 IA：侧栏 概览/项目/Agent/管理 四区 + 底部 zh/EN 即时
// 切换 + 路由出口）。B4-T3 起 概览/项目 已是真实页面，侧栏导航随页面票
// 逐项点亮；B4-T6 起管理四页（机密/审计/升级/用户）已实现，且管理区四入口
// 按 `/auth/me` 的 is_admin 门控——仅全局 admin 可见（非 admin 既不渲染入口，
// 直访 URL 由路由守卫兜底回首页）。pipeline 编辑仍占位。登出后整页回登录页：
// 清 cookie + 清认证态（401 回调的 redirect 不适用，登出是主动离开，不回跳原目标）。

import { computed } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { currentLocale, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'

const { t } = useI18n()
const router = useRouter()
const auth = useAuthStore()

const locale = computed(() => currentLocale())
const isAuthed = computed(() => auth.isAuthed)
const isAdmin = computed(() => auth.user?.isAdmin === true)
const username = computed(() => auth.user?.username ?? '')

/** 侧栏导航项（ADR-0020 四区；页面未实现者也列出，点击落占位页）。 */
type NavName =
  | 'overview'
  | 'projects'
  | 'agents'
  | 'admin-secrets'
  | 'admin-audit'
  | 'admin-upgrade'
  | 'admin-users'

/** 主区导航（全员可见）。 */
const mainNav: { name: NavName; labelKey: string }[] = [
  { name: 'overview', labelKey: 'routes.overview' },
  { name: 'projects', labelKey: 'routes.projects' },
  { name: 'agents', labelKey: 'routes.agents' },
]

/** 管理区导航（全局 admin 专属，票 B4-T6：is_admin 门控）。 */
const adminNav: { name: NavName; labelKey: string }[] = [
  { name: 'admin-secrets', labelKey: 'routes.adminSecrets' },
  { name: 'admin-audit', labelKey: 'routes.adminAudit' },
  { name: 'admin-upgrade', labelKey: 'routes.adminUpgrade' },
  { name: 'admin-users', labelKey: 'routes.adminUsers' },
]

function toggleLocale(): void {
  setLocale(locale.value === 'zh-CN' ? 'en-US' : 'zh-CN')
}

function go(name: NavName): void {
  void router.push({ name })
}

async function signOut(): Promise<void> {
  await auth.logout()
  await router.replace({ name: 'login' })
}
</script>

<template>
  <div class="app-shell">
    <aside v-if="isAuthed" class="app-sidebar">
      <nav class="sidebar-nav">
        <button
          v-for="item in mainNav"
          :key="item.name"
          type="button"
          class="sidebar-link"
          :class="{ active: $route.name === item.name }"
          @click="go(item.name)"
        >
          {{ t(item.labelKey) }}
        </button>
        <template v-if="isAdmin">
          <span class="sidebar-sep" aria-hidden="true"></span>
          <button
            v-for="item in adminNav"
            :key="item.name"
            type="button"
            class="sidebar-link"
            :class="{ active: $route.name === item.name }"
            @click="go(item.name)"
          >
            {{ t(item.labelKey) }}
          </button>
        </template>
      </nav>
    </aside>

    <div class="app-body">
      <main class="app-main">
        <RouterView />
      </main>

      <footer class="app-footer">
        <div v-if="isAuthed" class="footer-user">
          <span class="footer-username">{{ username }}</span>
          <button type="button" class="footer-logout" @click="signOut">
            {{ t('auth.logout') }}
          </button>
        </div>
        <button type="button" class="lang-switch" @click="toggleLocale">
          {{ t('app.langSwitch') }}
        </button>
      </footer>
    </div>
  </div>
</template>
