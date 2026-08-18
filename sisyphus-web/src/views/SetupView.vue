<script setup lang="ts">
// 初始化引导位（ADR-0010/0020，票 B4-T2 细化三步 wizard；B4-T1 立占位）。
// 三步（管理员 -> 首个 Agent -> 首个项目）各自可跳过、均带 CLI 等价提示
// 的完整实现归 B4-T2；本骨架只落「引导位」语义与空库说明文案。

import { useI18n } from 'vue-i18n'

const { t } = useI18n()
</script>

<template>
  <div class="setup-page">
    <h1>{{ t('app.name') }}</h1>
    <h2>{{ t('setup.title') }}</h2>
    <p>{{ t('setup.intro') }}</p>
    <p class="setup-note">{{ t('setup.comingSoon') }}</p>
  </div>
</template>
