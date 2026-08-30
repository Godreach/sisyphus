<script setup lang="ts">
// 应用壳（spec #99：prototype/ 设计稿 1:1 落地）：232px 深侧栏（Logo +
// 工作台/流水线/构建机 三项导航 + 底部用户卡）+ 60px 白顶栏（页面标题 +
// 搜索框/主按钮/语言切换）+ #F5F5F7 内容区。
//
// - 导航严格三项（spec 验收口径）；管理四页入口收编进用户卡下拉菜单，
//   仅全局 admin 可见，直访 URL 由路由守卫兜底（guards.ts 不变）。
// - 顶栏搜索框（流水线/构建机页）经 `?q=` 查询参数驱动页面过滤（250ms
//   防抖 replace），主按钮走各页既有创建流（`?create=1`）。
// - 窄屏（<768px）侧栏折叠为 NDrawer 抽屉（#87 行为保留）。
// - 未认证（登录/初始化引导）：无壳居中布局，无壳内开关（主题/语言切换
//   在登录壳不出现）。

import { computed, h, onBeforeUnmount, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { NButton, NDropdown, NIcon, NSwitch } from 'naive-ui'
import type { DropdownOption } from 'naive-ui'
import {
  LogOutOutline,
  Menu as MenuIcon,
  LockClosed,
  DocumentText,
  CloudUpload,
  People,
} from '@vicons/ionicons5'

import { currentLocale, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'
import { useDarkMode } from '@/composables/useDarkMode'
import { useBreakpoint } from '@/composables/useBreakpoint'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const auth = useAuthStore()
const { theme, themeOverrides } = useDarkMode()
const { isNarrow } = useBreakpoint()

const locale = computed(() => currentLocale())
const isAuthed = computed(() => auth.isAuthed)
const isAdmin = computed(() => auth.user?.isAdmin === true)
const username = computed(() => auth.user?.username ?? '')
/** 用户名首字符 → 头像（原型形态：渐变圆底 + 首字）。 */
const avatarChar = computed(() => (username.value ? username.value.charAt(0).toUpperCase() : '?'))

const drawerOpen = ref(false)

// ===== 侧栏宽度拖拽（可调宽 + 持久化；窄屏抽屉不适用） =====

const SIDEBAR_WIDTH_KEY = 'sisyphus-sidebar-width'
const SIDEBAR_MIN = 200
const SIDEBAR_MAX = 400
const SIDEBAR_DEFAULT = 232

function readSidebarWidth(): number {
  try {
    const v = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY))
    if (Number.isFinite(v) && v >= SIDEBAR_MIN && v <= SIDEBAR_MAX) return Math.round(v)
  } catch {
    // localStorage 不可用（隐私模式等）：回落默认宽度。
  }
  return SIDEBAR_DEFAULT
}

const sidebarWidth = ref(readSidebarWidth())
const sidebarDragging = ref(false)

function persistSidebarWidth(): void {
  try {
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth.value))
  } catch {
    // 同上：不可持久化时宽度仅本页生效。
  }
}

let dragStartX = 0
let dragStartWidth = 0

function onResizePointerDown(e: PointerEvent): void {
  dragStartX = e.clientX
  dragStartWidth = sidebarWidth.value
  sidebarDragging.value = true
  window.addEventListener('pointermove', onResizePointerMove)
  window.addEventListener('pointerup', onResizePointerUp)
  document.body.classList.add('sidebar-resizing')
  e.preventDefault()
}

function onResizePointerMove(e: PointerEvent): void {
  sidebarWidth.value = Math.min(
    SIDEBAR_MAX,
    Math.max(SIDEBAR_MIN, Math.round(dragStartWidth + (e.clientX - dragStartX))),
  )
}

function onResizePointerUp(): void {
  sidebarDragging.value = false
  window.removeEventListener('pointermove', onResizePointerMove)
  window.removeEventListener('pointerup', onResizePointerUp)
  document.body.classList.remove('sidebar-resizing')
  persistSidebarWidth()
}

/** 键盘微调（焦点在手柄上：←/→ 每次 16px）。 */
function onResizeKeydown(e: KeyboardEvent): void {
  if (e.key === 'ArrowLeft') {
    sidebarWidth.value = Math.max(SIDEBAR_MIN, sidebarWidth.value - 16)
    persistSidebarWidth()
    e.preventDefault()
  } else if (e.key === 'ArrowRight') {
    sidebarWidth.value = Math.min(SIDEBAR_MAX, sidebarWidth.value + 16)
    persistSidebarWidth()
    e.preventDefault()
  }
}

/** 双击手柄 → 复位默认宽度。 */
function onResizeDblClick(): void {
  sidebarWidth.value = SIDEBAR_DEFAULT
  persistSidebarWidth()
}

onBeforeUnmount(() => {
  window.removeEventListener('pointermove', onResizePointerMove)
  window.removeEventListener('pointerup', onResizePointerUp)
  document.body.classList.remove('sidebar-resizing')
})

// ===== 侧栏导航（严格三项；详情路由高亮所属主项） =====

type NavKey = 'workbench' | 'pipelines' | 'machines'

const NAV_ICONS: Record<NavKey, string> = {
  workbench:
    '<svg width="20" height="20" viewBox="0 0 16 16" fill="currentColor" xmlns="http://www.w3.org/2000/svg"><rect x="1" y="1" width="6" height="6" rx="1.5"/><rect x="9" y="1" width="6" height="6" rx="1.5"/><rect x="1" y="9" width="6" height="6" rx="1.5"/><rect x="9" y="9" width="6" height="6" rx="1.5"/></svg>',
  pipelines:
    '<svg width="20" height="20" viewBox="0 0 16 16" fill="currentColor" xmlns="http://www.w3.org/2000/svg"><rect x="11.5" y="6" width="3.5" height="4" rx="1"/><rect x="6.25" y="6" width="3.5" height="4" rx="1"/><rect x="1" y="6" width="3.5" height="4" rx="1"/></svg>',
  machines:
    '<svg width="20" height="20" viewBox="0 0 16 16" fill="currentColor" xmlns="http://www.w3.org/2000/svg"><rect x="3" y="5" width="10" height="8" rx="2"/><rect fill="#1D1D1F" x="6.5" y="8" width="3" height="3" rx="1"/><rect x="7.2" y="2" width="1.6" height="3" rx="0.8"/><circle cx="8" cy="2" r="1.2"/><rect x="1" y="8" width="2" height="3" rx="1"/><rect x="13" y="8" width="2" height="3" rx="1"/></svg>',
}

const navItems: { key: NavKey; routeName: string; labelKey: string }[] = [
  { key: 'workbench', routeName: 'overview', labelKey: 'nav.workbench' },
  { key: 'pipelines', routeName: 'pipelines', labelKey: 'nav.pipelines' },
  { key: 'machines', routeName: 'machines', labelKey: 'nav.machines' },
]

/** 当前路由 → 高亮主项（项目/构建/编辑器归流水线；Agent 详情归构建机）。 */
const activeNav = computed<NavKey | ''>(() => {
  switch (route.name) {
    case 'overview':
      return 'workbench'
    case 'pipelines':
    case 'projects':
    case 'project-detail':
    case 'pipeline-edit':
    case 'build-list':
    case 'build-detail':
      return 'pipelines'
    case 'machines':
    case 'agent-detail':
      return 'machines'
    default:
      return ''
  }
})

function goNav(name: string): void {
  drawerOpen.value = false
  void router.push({ name })
}

// ===== 用户卡下拉（管理四页入口收编 + 登出） =====

function renderDropdownIcon(icon: ReturnType<typeof import('vue').defineComponent>) {
  return () => h(NIcon, { component: icon })
}

/** 管理入口仅全局 admin 可见；非 admin 下拉只有登出。 */
const userMenuOptions = computed<DropdownOption[]>(() => {
  const opts: DropdownOption[] = []
  if (isAdmin.value) {
    opts.push(
      { key: 'admin-secrets', label: t('routes.adminSecrets'), icon: renderDropdownIcon(LockClosed) },
      { key: 'admin-audit', label: t('routes.adminAudit'), icon: renderDropdownIcon(DocumentText) },
      { key: 'admin-upgrade', label: t('routes.adminUpgrade'), icon: renderDropdownIcon(CloudUpload) },
      { key: 'admin-users', label: t('routes.adminUsers'), icon: renderDropdownIcon(People) },
      { type: 'divider', key: 'd1' },
    )
  }
  opts.push({ key: 'logout', label: t('auth.logout'), icon: renderDropdownIcon(LogOutOutline) })
  return opts
})

function handleUserMenuSelect(key: string): void {
  if (key === 'logout') {
    void signOut()
    return
  }
  void router.push({ name: key })
}

async function signOut(): Promise<void> {
  drawerOpen.value = false
  await auth.logout()
  await router.replace({ name: 'login' })
}

// ===== 顶栏 =====

/** 顶栏标题 = 路由 meta.title（i18n 键）；公开/404 页回落应用名。 */
const titleText = computed(() => {
  const key = route.meta.title
  return typeof key === 'string' ? t(key) : t('app.name')
})

/** 搜索框只在流水线/构建机两页出现（原型页二/三顶栏形态）。 */
const showSearch = computed(() => route.name === 'pipelines' || route.name === 'machines')

const searchQuery = ref(typeof route.query.q === 'string' ? route.query.q : '')
let searchTimer: ReturnType<typeof setTimeout> | undefined

function onSearchInput(value: string): void {
  searchQuery.value = value
  clearTimeout(searchTimer)
  searchTimer = setTimeout(() => {
    void router.replace({
      query: { ...route.query, q: value === '' ? undefined : value },
    })
  }, 250)
}

// 浏览器后退/外部改参时回填输入框。
watch(
  () => route.query.q,
  (q) => {
    const s = typeof q === 'string' ? q : ''
    if (s !== searchQuery.value) searchQuery.value = s
  },
)

/** 主按钮：流水线页 → 新建流水线（既有项目创建流）；构建机页 → 接入构建机
 *  （Agent 管理面全局 admin 专属，非 admin 不渲染）。 */
const ctaLabel = computed(() => {
  if (route.name === 'pipelines') return t('plines.newPipeline')
  if (route.name === 'machines' && isAdmin.value) return t('agents.accessMachine')
  return ''
})

function onCta(): void {
  if (route.name === 'pipelines') {
    void router.push({ name: 'projects', query: { create: '1' } })
    return
  }
  if (route.name === 'machines') {
    void router.push({ query: { ...route.query, create: '1' } })
  }
}

const isZh = computed(() => locale.value === 'zh-CN')

function toggleLocale(): void {
  setLocale(locale.value === 'zh-CN' ? 'en-US' : 'zh-CN')
}
</script>

<template>
  <n-config-provider :theme="theme" :theme-overrides="themeOverrides">
    <!-- useMessage 注入源（SetupView/PipelinesView/AgentListView 的 toast 反馈
         依赖此 provider；缺它时 useMessage() 返回 undefined、toast 静默失效）。 -->
    <n-message-provider>
      <!-- 未认证：无壳居中布局（登录/初始化引导/404）。 -->
      <template v-if="!isAuthed">
        <div class="app-bare">
          <main class="app-main">
            <RouterView />
          </main>
        </div>
      </template>

      <template v-else>
        <div class="app-shell">
          <!-- 桌面端深侧栏（prototype 外壳；宽度可拖拽调整）。 -->
          <aside
            v-if="!isNarrow"
            class="app-sidebar"
            :class="{ dragging: sidebarDragging }"
            :style="{ flex: `0 0 ${sidebarWidth}px`, width: `${sidebarWidth}px` }"
          >
            <div class="sidebar-logo">
              <svg width="20" height="20" viewBox="0 0 20 20" xmlns="http://www.w3.org/2000/svg"><rect fill="#2997FF" x="1" y="1" width="7" height="7" rx="2"/><rect fill="#2997FF" x="12" y="1" width="7" height="7" rx="2"/><rect fill="#2997FF" x="1" y="12" width="7" height="7" rx="2"/><rect fill="#5E5CE6" x="12" y="12" width="7" height="7" rx="2"/></svg>
              <span>{{ t('app.name') }}</span>
            </div>
            <nav class="sidebar-nav">
              <button
                v-for="item in navItems"
                :key="item.key"
                type="button"
                class="nav-item"
                :class="{ active: activeNav === item.key }"
                :data-testid="`nav-${item.key}`"
                @click="goNav(item.routeName)"
              >
                <span v-html="NAV_ICONS[item.key]" />
                <span>{{ t(item.labelKey) }}</span>
              </button>
            </nav>
            <div class="sidebar-spacer" />
            <n-dropdown
              trigger="click"
              :options="userMenuOptions"
              @select="handleUserMenuSelect"
            >
              <button type="button" class="sidebar-user" data-testid="sidebar-user">
                <span class="user-avatar">{{ avatarChar }}</span>
                <span class="user-meta">
                  <span class="user-name">{{ username }}</span>
                  <span class="user-role">{{ isAdmin ? t('userCard.roleAdmin') : t('userCard.roleMember') }}</span>
                </span>
                <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5" xmlns="http://www.w3.org/2000/svg" style="color:#A1A1A6"><path d="M2 3.5 L5 6.5 L8 3.5"/></svg>
              </button>
            </n-dropdown>
            <!-- 拖拽调宽手柄（右缘；双击复位，←/→ 微调）。 -->
            <div
              class="sidebar-resizer"
              role="separator"
              aria-orientation="vertical"
              :aria-valuenow="sidebarWidth"
              :aria-label="t('app.sidebarResize')"
              tabindex="0"
              @pointerdown="onResizePointerDown"
              @dblclick="onResizeDblClick"
              @keydown="onResizeKeydown"
            />
          </aside>

          <div class="app-body">
            <!-- 顶栏：窄屏汉堡 + 标题 + 搜索/主按钮/语言切换。 -->
            <header class="app-topbar">
              <div class="app-topbar-left">
                <n-button v-if="isNarrow" quaternary size="small" @click="drawerOpen = true">
                  <template #icon>
                    <n-icon :component="MenuIcon" />
                  </template>
                </n-button>
                <span class="app-topbar-title">{{ titleText }}</span>
              </div>
              <div class="app-topbar-right">
                <div v-if="showSearch" class="topbar-search">
                  <svg width="14" height="14" viewBox="0 0 14 14" xmlns="http://www.w3.org/2000/svg"><circle cx="6" cy="6" r="4.5" fill="none" stroke="#86868B" stroke-width="1.5"/><rect x="9.5" y="10.5" width="3.5" height="1.5" rx="0.75" fill="#86868B"/></svg>
                  <input
                    :value="searchQuery"
                    type="text"
                    :placeholder="route.name === 'pipelines' ? t('plines.searchPlaceholder') : t('agents.searchPlaceholder')"
                    data-testid="topbar-search"
                    @input="onSearchInput(($event.target as HTMLInputElement).value)"
                  />
                </div>
                <n-button v-if="ctaLabel" type="primary" size="small" data-testid="topbar-cta" @click="onCta">
                  <template #icon>
                    <svg width="12" height="12" viewBox="0 0 12 12" xmlns="http://www.w3.org/2000/svg"><rect x="5.25" y="1" width="1.5" height="10" rx="0.75" fill="currentColor"/><rect x="1" y="5.25" width="10" height="1.5" rx="0.75" fill="currentColor"/></svg>
                  </template>
                  {{ ctaLabel }}
                </n-button>
                <n-switch :value="isZh" size="small" @update:value="toggleLocale">
                  <template #checked>中</template>
                  <template #unchecked>EN</template>
                </n-switch>
              </div>
            </header>

            <!-- 窄屏 NDrawer 抽屉导航（深侧栏同款 + 登出）。 -->
            <n-drawer v-model:show="drawerOpen" :width="240" placement="left">
              <n-drawer-content :title="t('app.name')" closable>
                <nav class="sidebar-nav drawer-nav">
                  <button
                    v-for="item in navItems"
                    :key="item.key"
                    type="button"
                    class="nav-item drawer-nav-item"
                    :class="{ active: activeNav === item.key }"
                    @click="goNav(item.routeName)"
                  >
                    <span v-html="NAV_ICONS[item.key]" />
                    <span>{{ t(item.labelKey) }}</span>
                  </button>
                </nav>
                <div v-if="isAdmin" class="drawer-admin">
                  <p class="nav-group-label">{{ t('userCard.adminMenu') }}</p>
                  <button
                    v-for="item in [
                      { name: 'admin-secrets', labelKey: 'routes.adminSecrets' },
                      { name: 'admin-audit', labelKey: 'routes.adminAudit' },
                      { name: 'admin-upgrade', labelKey: 'routes.adminUpgrade' },
                      { name: 'admin-users', labelKey: 'routes.adminUsers' },
                    ]"
                    :key="item.name"
                    type="button"
                    class="nav-item drawer-nav-item"
                    @click="goNav(item.name)"
                  >
                    <span>{{ t(item.labelKey) }}</span>
                  </button>
                </div>
                <div class="drawer-user">
                  <span class="user-avatar">{{ avatarChar }}</span>
                  <span class="user-meta">
                    <span class="user-name drawer-user-name">{{ username }}</span>
                    <span class="user-role drawer-user-role">{{ isAdmin ? t('userCard.roleAdmin') : t('userCard.roleMember') }}</span>
                  </span>
                  <button type="button" class="user-logout" data-testid="drawer-logout" @click="signOut">
                    <n-icon :component="LogOutOutline" />
                  </button>
                </div>
              </n-drawer-content>
            </n-drawer>

            <main class="app-main">
              <RouterView />
            </main>
          </div>
        </div>
      </template>
    </n-message-provider>
  </n-config-provider>
</template>

<style scoped>
/* 顶栏搜索框（prototype 形态：页面底色圆角框 + 放大镜）。 */
.topbar-search {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 220px;
  height: 34px;
  background: var(--sisy-color-bg);
  border-radius: var(--sisy-radius);
  padding: 0 12px;
}

.topbar-search input {
  border: none;
  outline: none;
  background: transparent;
  font-family: inherit;
  font-size: 13px;
  color: var(--sisy-color-text);
  width: 100%;
}

.topbar-search input::placeholder {
  color: var(--sisy-color-text-secondary);
}

/* 抽屉内的导航/用户卡：深色样式在浅色抽屉里取中性变体。 */
.drawer-nav {
  padding: 0;
}

.drawer-nav-item {
  color: var(--sisy-color-text);
}

.drawer-nav-item svg {
  color: var(--sisy-color-primary);
}

.drawer-nav-item:hover,
.drawer-nav-item.active {
  background: var(--sisy-color-bg);
}

.drawer-admin {
  margin-top: 8px;
  border-top: 1px solid var(--sisy-color-border);
}

.drawer-user {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 16px;
  padding-top: 12px;
  border-top: 1px solid var(--sisy-color-border);
}

.drawer-user .user-meta {
  display: flex;
}

.drawer-user-name {
  color: var(--sisy-color-text);
}

.drawer-user-role {
  color: var(--sisy-color-text-secondary);
}

.drawer-user .user-logout {
  color: var(--sisy-color-text-secondary);
}

.drawer-user .user-logout:hover {
  color: var(--sisy-color-text);
  background: var(--sisy-color-bg);
}
</style>
