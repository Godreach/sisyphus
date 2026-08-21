<script setup lang="ts">
// 应用壳（ADR-0020 IA：侧栏 概览/项目/Agent/管理 四区 + 底部 zh/EN 即时
// 切换 + 路由出口）。B4-T3 起 概览/项目 已是真实页面，侧栏导航随页面票
// 逐项点亮；B4-T6 起管理四页（机密/审计/升级/用户）已实现，且管理区四入口
// 按 `/auth/me` 的 is_admin 门控——仅全局 admin 可见（非 admin 既不渲染入口，
// 直访 URL 由路由守卫兜底回首页）。pipeline 编辑仍占位。登出后整页回登录页：
// 清 cookie + 清认证态（401 回调的 redirect 不适用，登出是主动离开，不回跳原目标）。

import { computed, h } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import type { MenuMixedOption } from 'naive-ui'
import {
  Home,
  FolderOpen,
  Server,
  LockClosed,
  DocumentText,
  CloudUpload,
  People,
} from '@vicons/ionicons5'

import { currentLocale, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'
import { useDarkMode } from '@/composables/useDarkMode'

const { t } = useI18n()
const router = useRouter()
const auth = useAuthStore()
const { theme, themeOverrides } = useDarkMode()

const locale = computed(() => currentLocale())
const isAuthed = computed(() => auth.isAuthed)
const isAdmin = computed(() => auth.user?.isAdmin === true)
const username = computed(() => auth.user?.username ?? '')

type NavName =
  | 'overview'
  | 'projects'
  | 'agents'
  | 'admin-secrets'
  | 'admin-audit'
  | 'admin-upgrade'
  | 'admin-users'

/** 渲染 ionicons5 图标组件（NMenu icon 需要 () => VNode）。 */
function renderIcon(icon: ReturnType<typeof import('vue').defineComponent>) {
  return () => h(icon, { style: 'width: 18px; height: 18px' })
}

/** 构建 NMenu 选项：主区 + 管理区（admin 仅对全局 admin 可见）。 */
const menuOptions = computed<MenuMixedOption[]>(() => {
  const groups: MenuMixedOption[] = [
    {
      type: 'group',
      label: () => t('nav.main'),
      key: 'group-main',
      children: [
        { key: 'overview', label: () => t('routes.overview'), icon: renderIcon(Home) },
        { key: 'projects', label: () => t('routes.projects'), icon: renderIcon(FolderOpen) },
        { key: 'agents', label: () => t('routes.agents'), icon: renderIcon(Server) },
      ],
    },
  ]

  if (isAdmin.value) {
    groups.push({
      type: 'group',
      label: () => t('nav.admin'),
      key: 'group-admin',
      children: [
        { key: 'admin-secrets', label: () => t('routes.adminSecrets'), icon: renderIcon(LockClosed) },
        { key: 'admin-audit', label: () => t('routes.adminAudit'), icon: renderIcon(DocumentText) },
        { key: 'admin-upgrade', label: () => t('routes.adminUpgrade'), icon: renderIcon(CloudUpload) },
        { key: 'admin-users', label: () => t('routes.adminUsers'), icon: renderIcon(People) },
      ],
    })
  }

  return groups
})

const activeKey = computed(() => {
  const name = router.currentRoute.value.name
  return (typeof name === 'string' ? name : null) as string | null
})

function handleMenuUpdate(key: string) {
  void router.push({ name: key as NavName })
}

function toggleLocale(): void {
  setLocale(locale.value === 'zh-CN' ? 'en-US' : 'zh-CN')
}

async function signOut(): Promise<void> {
  await auth.logout()
  await router.replace({ name: 'login' })
}
</script>

<template>
  <n-config-provider :theme="theme" :theme-overrides="themeOverrides">
    <div class="app-shell">
      <aside v-if="isAuthed" class="app-sidebar">
        <n-menu
          :options="menuOptions"
          :value="activeKey"
          :indent="24"
          @update:value="handleMenuUpdate"
        />
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
  </n-config-provider>
</template>
