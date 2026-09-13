<script setup lang="ts">
// 认证面共享卡片（ADR-0023 base 组件，票 #112）：登录页与初始化引导页
// 同源的完整品牌 logo + 应用名 h1 + 定稿卡片外壳（surface 底 +
// 12px 圆角 + 边框 + 轻阴影，浅/深自适应；窄屏内边距收紧）。
//
// 抽出两页重复的品牌标记与卡片外壳——遵循 ADR-0023「跨页面复用基础组件
// 统一从 @/components/base/ 引用」。404 不走本组件（壳内/壳外就地居中、
// 无品牌首屏，由 NResult 自带视觉）。各页通过具名根 class（login-card /
// setup-card，经 Vue 属性透传落到本根）做测试钩子与页面专属微调。
//
// 品牌图形与侧栏 logo 同源（App.vue sidebar-logo），认证卡片直接复用完整
// wordmark，避免登录页与应用壳出现两套品牌表达。

import { useI18n } from 'vue-i18n'
import { useDarkMode } from '@/composables/useDarkMode'

const { t } = useI18n()
const { isDark } = useDarkMode()
</script>

<template>
  <div class="auth-card">
    <div class="auth-brand">
      <img
        class="auth-logo-image"
        :src="isDark ? '/sisyphus-logo.svg' : '/sisyphus-logo-dark.svg'"
        alt=""
        aria-hidden="true"
        width="1360"
        height="180"
      />
      <h1 class="auth-title sr-only">{{ t('app.name') }}</h1>
    </div>
    <!-- 页面专属内容（表单/步骤/凭据等）经默认插槽注入。 -->
    <slot />
  </div>
</template>

<style scoped>
/* 定稿设计语言卡片：surface 底 + 12px 圆角 + 边框 + 轻阴影（认证面是孤立
   居中面，加阴影/边框帮助在裸底上读出「卡片」层级；壳内 sisy-card 无此需要）。 */
.auth-card {
  background: var(--sisy-color-surface);
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius-card);
  box-shadow: 0 2px 16px rgba(0, 0, 0, 0.06);
  padding: 32px;
}

@media (prefers-color-scheme: dark) {
  .auth-card {
    box-shadow: 0 2px 16px rgba(0, 0, 0, 0.4);
  }
}

.auth-brand {
  display: flex;
  justify-content: center;
  margin-bottom: 28px;
}

.auth-logo-image {
  display: block;
  width: 100%;
  max-width: 360px;
  height: auto;
  /* 与侧栏一致，抵消 SVG viewBox 右侧透明留白，让视觉内容居中。 */
  transform: translateX(4.6%);
  transform-origin: center;
}

.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border: 0;
}

/* 窄屏：卡片内边距收紧（页面根的 padding 由各页 scoped 控制）。 */
@media (max-width: 767px) {
  .auth-card {
    padding: 24px 20px;
  }
}
</style>
