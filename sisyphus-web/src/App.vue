<script setup lang="ts">
// 应用壳（ADR-0020 IA：侧栏 概览/项目/Agent/管理 四区 + 底部 zh/EN 即时
// 切换 + 路由出口）。B4-T3 起 概览/项目 已是真实页面，侧栏导航随页面票
// 逐项点亮；B4-T6 起管理四页（机密/审计/升级/用户）已实现，且管理区四入口
// 按 `/auth/me` 的 is_admin 门控——仅全局 admin 可见（非 admin 既不渲染入口，
// 直访 URL 由路由守卫兜底回首页）。pipeline 编辑仍占位。登出后整页回登录页：
// 清 cookie + 清认证态（401 回调的 redirect 不适用，登出是主动离开，不回跳原目标）。
// #87: 窄屏（<768px）侧栏折叠为 NDrawer 抽屉式展开，Footer 用 Naive UI 组件重写。

import { computed, h, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import type { MenuOption } from 'naive-ui'
import {
  Home,
  FolderOpen,
  Server,
  LockClosed,
  DocumentText,
  CloudUpload,
  People,
  Menu as MenuIcon,
} from '@vicons/ionicons5'

import { currentLocale, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'
import { useDarkMode } from '@/composables/useDarkMode'
import { useBreakpoint } from '@/composables/useBreakpoint'

const { t } = useI18n()
const router = useRouter()
const auth = useAuthStore()
const { theme, themeOverrides } = useDarkMode()
const { isNarrow } = useBreakpoint()

const locale = computed(() => currentLocale())
const isAuthed = computed(() => auth.isAuthed)
const isAdmin = computed(() => auth.user?.isAdmin === true)
const username = computed(() => auth.user?.username ?? '')

const drawerOpen = ref(false)

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
const menuOptions = computed<MenuOption[]>(() => {
  const groups: MenuOption[] = [
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
  drawerOpen.value = false
  void router.push({ name: key as NavName })
}

const isZh = computed(() => locale.value === 'zh-CN')

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
    <!-- useMessage 注入源（SetupView/ProjectsView/AgentListView 的 toast 反馈
         依赖此 provider；缺它时 useMessage() 返回 undefined、toast 静默失效）。 -->
    <n-message-provider>
      <div class="app-shell">
        <!-- 桌面端侧栏 -->
        <aside v-if="isAuthed && !isNarrow" class="app-sidebar">
          <n-menu
            :options="menuOptions"
            :value="activeKey"
            :indent="24"
            @update:value="handleMenuUpdate"
          />
        </aside>

        <div class="app-body">
          <!-- 窄屏顶部栏：汉堡按钮 -->
          <header v-if="isAuthed && isNarrow" class="app-topbar">
            <n-button quaternary @click="drawerOpen = true">
              <template #icon>
                <n-icon :component="MenuIcon" />
              </template>
            </n-button>
            <span class="app-topbar-title">{{ t('app.name') }}</span>
          </header>

          <!-- 窄屏 NDrawer 抽屉导航 -->
          <n-drawer v-model:show="drawerOpen" :width="240" placement="left">
            <n-drawer-content :title="t('app.name')" closable>
              <n-menu
                :options="menuOptions"
                :value="activeKey"
                :indent="24"
                @update:value="handleMenuUpdate"
              />
            </n-drawer-content>
          </n-drawer>

          <main class="app-main">
            <RouterView />
          </main>

          <footer class="app-footer">
            <div v-if="isAuthed" class="footer-user">
              <n-text strong>{{ username }}</n-text>
              <n-button size="small" @click="signOut">
                {{ t('auth.logout') }}
              </n-button>
            </div>
            <n-switch :value="isZh" @update:value="toggleLocale">
              <template #checked>中</template>
              <template #unchecked>EN</template>
            </n-switch>
          </footer>
        </div>
      </div>
    </n-message-provider>
  </n-config-provider>
</template>
