import type { GlobalThemeOverrides } from 'naive-ui'

// 主色 indigo-600 系（shadcn/GitHub 中性专业风）；深色模式主色整体上移一档
// 提对比（indigo-500 起），底色走 zinc 系纯中性灰，不带蓝调。
const commonOverrides = {
  primaryColor: '#4f46e5',
  primaryColorHover: '#6366f1',
  primaryColorPressed: '#4338ca',
  primaryColorSuppl: '#4f46e5',
  successColor: '#18a058',
  successColorHover: '#36ad6a',
  successColorPressed: '#0c7a43',
  successColorSuppl: '#18a058',
  errorColor: '#d03050',
  errorColorHover: '#de5773',
  errorColorPressed: '#ab1f3f',
  errorColorSuppl: '#d03050',
  warningColor: '#f0a020',
  warningColorHover: '#fcb040',
  warningColorPressed: '#ca9416',
  warningColorSuppl: '#f0a020',
  borderRadius: '6px',
  borderRadiusSmall: '4px',
  fontFamily:
    '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif',
  fontFamilyMono:
    '"JetBrains Mono", "Fira Code", "Fira Mono", "Roboto Mono", Menlo, Monaco, Consolas, monospace',
}

export const themeOverrides: GlobalThemeOverrides = {
  common: commonOverrides,
}

export const darkThemeOverrides: GlobalThemeOverrides = {
  common: {
    ...commonOverrides,
    primaryColor: '#6366f1',
    primaryColorHover: '#818cf8',
    primaryColorPressed: '#4f46e5',
    primaryColorSuppl: '#6366f1',
    bodyColor: '#0c0c0e',
    cardColor: '#18181b',
    modalColor: '#18181b',
    popoverColor: '#202023',
    tableColor: '#18181b',
    inputColor: '#0f0f12',
    actionColor: '#141417',
    textColor1: '#fafafa',
    textColor2: '#d4d4d8',
    textColor3: '#a1a1aa',
    borderColor: '#27272a',
    dividerColor: '#27272a',
    hoverColor: '#232329',
  },
}
