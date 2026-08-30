// 用户卡下拉菜单（票 #104 裁定 G3/G4）：管理入口（admin）+ 语言/主题二级
// 子菜单 + 登出。选项构建与选中分发抽成纯函数（naive-ui 二级菜单的悬停
// 展开是库内行为，jsdom 合成事件无法触达——测试打在本模块与真实 i18n/
// 主题副作用上，App.vue 只负责渲染）。

import type { DropdownOption } from 'naive-ui'

import type { ThemePreference } from '@/composables/useDarkMode'
import type { Locale } from '@/i18n'

/** 菜单键前缀（二级子菜单叶子的动作编码：`<动作>:<参数>`）。 */
export const LANG_KEY_PREFIX = 'lang:'
export const THEME_KEY_PREFIX = 'theme:'

export interface UserMenuContext {
  isAdmin: boolean
  locale: Locale
  themePreference: ThemePreference
  /** i18n 词条（App 单实例消费，直接传 t 避免 vue-i18n 依赖渗入纯函数）。 */
  labels: {
    adminSecrets: string
    adminAudit: string
    adminUpgrade: string
    adminUsers: string
    language: string
    theme: string
    themeSystem: string
    themeLight: string
    themeDark: string
    logout: string
  }
}

/** 当前项打勾（naive-ui 下拉无内建选中态，以 Checkmark 图标标注）。 */
function checkIcon(renderIcon: (icon: string) => DropdownOption['icon']): DropdownOption['icon'] {
  return renderIcon('check')
}

/** 构建用户卡下拉选项（纯函数：同入参同构）。图标键为 ICON_BY_KEY 的键。 */
export function buildUserMenuOptions(
  ctx: UserMenuContext,
  renderIcon: (icon: string) => DropdownOption['icon'],
): DropdownOption[] {
  const opts: DropdownOption[] = []
  if (ctx.isAdmin) {
    opts.push(
      { key: 'admin-secrets', label: ctx.labels.adminSecrets, icon: renderIcon('secrets') },
      { key: 'admin-audit', label: ctx.labels.adminAudit, icon: renderIcon('audit') },
      { key: 'admin-upgrade', label: ctx.labels.adminUpgrade, icon: renderIcon('upgrade') },
      { key: 'admin-users', label: ctx.labels.adminUsers, icon: renderIcon('users') },
      { type: 'divider', key: 'd1' },
    )
  }
  opts.push(
    {
      key: 'lang',
      label: ctx.labels.language,
      icon: renderIcon('language'),
      children: [
        { key: `${LANG_KEY_PREFIX}zh-CN`, label: '中文', icon: ctx.locale === 'zh-CN' ? checkIcon(renderIcon) : undefined },
        { key: `${LANG_KEY_PREFIX}en-US`, label: 'English', icon: ctx.locale === 'en-US' ? checkIcon(renderIcon) : undefined },
      ],
    },
    {
      key: 'theme',
      label: ctx.labels.theme,
      icon: renderIcon(ctx.themePreference === 'dark' ? 'moon' : 'sun'),
      children: [
        { key: `${THEME_KEY_PREFIX}system`, label: ctx.labels.themeSystem, icon: ctx.themePreference === 'system' ? checkIcon(renderIcon) : undefined },
        { key: `${THEME_KEY_PREFIX}light`, label: ctx.labels.themeLight, icon: ctx.themePreference === 'light' ? checkIcon(renderIcon) : undefined },
        { key: `${THEME_KEY_PREFIX}dark`, label: ctx.labels.themeDark, icon: ctx.themePreference === 'dark' ? checkIcon(renderIcon) : undefined },
      ],
    },
    { type: 'divider', key: 'd2' },
    { key: 'logout', label: ctx.labels.logout, icon: renderIcon('logout') },
  )
  return opts
}

/** 菜单键选中动作（纯分发：副作用经 actions 注入）。 */
export interface UserMenuActions {
  setLocale: (locale: Locale) => void
  setThemePreference: (pref: ThemePreference) => void
  navigate: (routeName: string) => void
  logout: () => void
}

export function applyUserMenuKey(key: string, actions: UserMenuActions): void {
  if (key === 'logout') {
    actions.logout()
    return
  }
  if (key.startsWith(LANG_KEY_PREFIX)) {
    actions.setLocale(key.slice(LANG_KEY_PREFIX.length) as Locale)
    return
  }
  if (key.startsWith(THEME_KEY_PREFIX)) {
    actions.setThemePreference(key.slice(THEME_KEY_PREFIX.length) as ThemePreference)
    return
  }
  actions.navigate(key)
}
