// 用户卡菜单纯模块测试（票 #104 裁定 G3/G4 二级子菜单形态）：
// - 选项结构：管理四页入口仅 admin / 语言与主题二级子菜单（当前项打勾）/ 登出
// - 键分发：lang:/theme:/导航/登出（副作用经 actions 注入；真实 setLocale/
//   setThemePreference 的效果归 App 壳/useDarkMode 测试，此处只验分发）
//
// 说明：naive-ui 二级菜单的悬停展开是库内行为，jsdom 合成事件无法触达
// （最小实验验证）——故 App.spec 只断言父项存在，叶子交互测在本模块。

import { describe, expect, it, vi } from 'vitest'

import { applyUserMenuKey, buildUserMenuOptions, type UserMenuActions } from '@/utils/userMenu'

const baseCtx = {
  isAdmin: false,
  locale: 'zh-CN' as const,
  themePreference: 'system' as const,
  labels: {
    adminSecrets: '机密',
    adminAudit: '审计日志',
    adminUpgrade: '构建机升级',
    adminUsers: '用户',
    language: '语言',
    theme: '主题',
    themeSystem: '跟随系统',
    themeLight: '浅色',
    themeDark: '深色',
    logout: '登出',
  },
}

describe('buildUserMenuOptions（二级子菜单结构）', () => {
  it('非 admin：语言/主题二级子菜单 + 登出，无管理入口', () => {
    const opts = buildUserMenuOptions(baseCtx, () => undefined)
    // divider 无 label（undefined 项），过滤后为三段结构。
    const labels = opts.map((o) => o.label).filter((l) => l != null)
    expect(labels).toEqual(['语言', '主题', '登出'])

    const lang = opts[0]!
    expect(lang.children).toHaveLength(2)
    expect(lang.children!.map((c) => c.key)).toEqual(['lang:zh-CN', 'lang:en-US'])

    const theme = opts[1]!
    expect(theme.children).toHaveLength(3)
    expect(theme.children!.map((c) => c.key)).toEqual(['theme:system', 'theme:light', 'theme:dark'])
  })

  it('admin：管理四页入口在前（divider 分隔），语言/主题/登出随后', () => {
    const opts = buildUserMenuOptions({ ...baseCtx, isAdmin: true }, () => undefined)
    const keys = opts.map((o) => ('key' in o ? o.key : null))
    expect(keys).toEqual([
      'admin-secrets',
      'admin-audit',
      'admin-upgrade',
      'admin-users',
      'd1',
      'lang',
      'theme',
      'd2',
      'logout',
    ])
  })

  it('当前语言/主题项打勾（icon 存在），其余项不打勾', () => {
    const rendered: string[] = []
    const opts = buildUserMenuOptions(
      { ...baseCtx, locale: 'en-US', themePreference: 'dark' },
      (key) => {
        rendered.push(key as string)
        return { render: () => null } as never
      },
    )
    const langChildren = opts[0]!.children!
    const themeChildren = opts[1]!.children!
    // English 与 深色 打勾；中文/跟随系统/浅色不打勾（icon undefined）。
    expect(langChildren[0]!.icon).toBeUndefined()
    expect(langChildren[1]!.icon).toBeDefined()
    expect(themeChildren[0]!.icon).toBeUndefined()
    expect(themeChildren[1]!.icon).toBeUndefined()
    expect(themeChildren[2]!.icon).toBeDefined()
    // 主题父项图标取 moon（深色高亮形态）。
    expect(rendered).toContain('moon')
  })
})

describe('applyUserMenuKey（键分发）', () => {
  function spyActions(): UserMenuActions & Record<'setLocale' | 'setThemePreference' | 'navigate' | 'logout', ReturnType<typeof vi.fn>> {
    return {
      setLocale: vi.fn(),
      setThemePreference: vi.fn(),
      navigate: vi.fn(),
      logout: vi.fn(),
    }
  }

  it('lang:/theme: 前缀解析为语言/主题动作', () => {
    const a = spyActions()
    applyUserMenuKey('lang:en-US', a)
    expect(a.setLocale).toHaveBeenCalledWith('en-US')
    applyUserMenuKey('theme:dark', a)
    expect(a.setThemePreference).toHaveBeenCalledWith('dark')
    applyUserMenuKey('theme:system', a)
    expect(a.setThemePreference).toHaveBeenCalledWith('system')
    expect(a.navigate).not.toHaveBeenCalled()
    expect(a.logout).not.toHaveBeenCalled()
  })

  it('logout 键走登出；其余键原样导航', () => {
    const a = spyActions()
    applyUserMenuKey('logout', a)
    expect(a.logout).toHaveBeenCalledTimes(1)
    applyUserMenuKey('admin-users', a)
    expect(a.navigate).toHaveBeenCalledWith('admin-users')
  })
})
