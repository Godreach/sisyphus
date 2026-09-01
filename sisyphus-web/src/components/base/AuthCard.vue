<script setup lang="ts">
// 认证面共享卡片（ADR-0023 base 组件，票 #112）：登录页与初始化引导页
// 同源的品牌标识（4 方块 logo + 应用名 h1）+ 定稿卡片外壳（surface 底 +
// 12px 圆角 + 边框 + 轻阴影，浅/深自适应；窄屏内边距收紧）。
//
// 抽出两页重复的品牌标记与卡片外壳——遵循 ADR-0023「跨页面复用基础组件
// 统一从 @/components/base/ 引用」。404 不走本组件（壳内/壳外就地居中、
// 无品牌首屏，由 NResult 自带视觉）。各页通过具名根 class（login-card /
// setup-card，经 Vue 属性透传落到本根）做测试钩子与页面专属微调。
//
// 品牌 4 方块与侧栏 logo 同源（App.vue sidebar-logo）：主色三块 + accent 一
// 块。侧栏底恒深、logo 取深色蓝；本卡片底随主题（浅底取浅色主色 token、深
// 底取深色主色 token），故填色走 var(--sisy-color-primary) 令其随浅/深自适应——
// 两面同源在「同形 + 同设计 Token 体系」，而非字面色值强等（侧栏恒深面用
// 深色蓝是正确取舍，认证卡片随主题面用主题主色亦是）。

import { useI18n } from 'vue-i18n'

const { t } = useI18n()
</script>

<template>
  <div class="auth-card">
    <div class="auth-brand">
      <svg class="auth-mark" width="32" height="32" viewBox="0 0 20 20" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
        <rect style="fill: var(--sisy-color-primary)" x="1" y="1" width="7" height="7" rx="2" />
        <rect style="fill: var(--sisy-color-primary)" x="12" y="1" width="7" height="7" rx="2" />
        <rect style="fill: var(--sisy-color-primary)" x="1" y="12" width="7" height="7" rx="2" />
        <rect style="fill: var(--sisy-color-accent)" x="12" y="12" width="7" height="7" rx="2" />
      </svg>
      <h1 class="auth-title">{{ t('app.name') }}</h1>
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
  align-items: center;
  gap: 12px;
  margin-bottom: 4px;
}

.auth-mark {
  flex-shrink: 0;
}

.auth-title {
  margin: 0;
  font-size: 20px;
  font-weight: 600;
  letter-spacing: 0.2px;
  color: var(--sisy-color-text);
}

/* 窄屏：卡片内边距收紧（页面根的 padding 由各页 scoped 控制）。 */
@media (max-width: 767px) {
  .auth-card {
    padding: 24px 20px;
  }
}
</style>
