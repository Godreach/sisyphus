<script setup lang="ts">
// 404（未命中路由，ADR-0020 SPA fallback 语义：未知路径在 UI 侧给提示）。

import { useI18n } from 'vue-i18n'

const { t } = useI18n()
</script>

<template>
  <div class="not-found-page">
    <h1>404</h1>
    <p>{{ t('errors.notFound') }}</p>
  </div>
</template>
