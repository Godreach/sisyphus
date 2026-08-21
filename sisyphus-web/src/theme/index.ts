import type { GlobalThemeOverrides } from 'naive-ui'

const commonOverrides = {
  primaryColor: '#2b5797',
  primaryColorHover: '#3a6bb5',
  primaryColorPressed: '#1e4070',
  primaryColorSuppl: '#2b5797',
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
    bodyColor: '#1a1a2e',
    cardColor: '#16213e',
    modalColor: '#16213e',
    popoverColor: '#16213e',
    tableColor: '#16213e',
    inputColor: '#0f3460',
    actionColor: '#16213e',
    textColor1: '#e0e0e0',
    textColor2: '#c0c0d0',
    textColor3: '#a0a0b0',
    borderColor: '#2a2a4a',
    dividerColor: '#2a2a4a',
    hoverColor: '#1e2a50',
  },
}
