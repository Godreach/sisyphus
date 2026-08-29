import type { GlobalThemeOverrides } from 'naive-ui'

// 视觉基线 = prototype/ 设计稿（spec #99）：主蓝 #0066CC 系、成功绿
// #1E8E3E、失败红 #FF3B30、重试橙 #FF9500、按钮圆角 8px。深色模式为同一
// 色板的深色变体（主色上移提对比，底色走中性深灰）。
const commonOverrides = {
  primaryColor: '#0066CC',
  primaryColorHover: '#0059B3',
  primaryColorPressed: '#004A99',
  primaryColorSuppl: '#0066CC',
  successColor: '#1E8E3E',
  successColorHover: '#28A64C',
  successColorPressed: '#17702F',
  successColorSuppl: '#1E8E3E',
  errorColor: '#FF3B30',
  errorColorHover: '#FF5B52',
  errorColorPressed: '#D70015',
  errorColorSuppl: '#FF3B30',
  warningColor: '#FF9500',
  warningColorHover: '#FFAB33',
  warningColorPressed: '#CA3400',
  warningColorSuppl: '#FF9500',
  borderRadius: '8px',
  borderRadiusSmall: '6px',
  fontFamily:
    '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", "Microsoft YaHei", "Helvetica Neue", Arial, "Noto Sans", sans-serif',
  fontFamilyMono:
    '"JetBrains Mono", "Fira Code", "Fira Mono", "Roboto Mono", Menlo, Monaco, Consolas, monospace',
}

export const themeOverrides: GlobalThemeOverrides = {
  common: commonOverrides,
}

export const darkThemeOverrides: GlobalThemeOverrides = {
  common: {
    ...commonOverrides,
    primaryColor: '#2997FF',
    primaryColorHover: '#55AAFF',
    primaryColorPressed: '#0A84FF',
    primaryColorSuppl: '#2997FF',
    successColor: '#30D158',
    successColorHover: '#32D74B',
    successColorPressed: '#28A64C',
    successColorSuppl: '#30D158',
    errorColor: '#FF453A',
    errorColorHover: '#FF6961',
    errorColorPressed: '#D70015',
    errorColorSuppl: '#FF453A',
    warningColor: '#FF9F0A',
    warningColorHover: '#FFB340',
    warningColorPressed: '#CA3400',
    warningColorSuppl: '#FF9F0A',
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
