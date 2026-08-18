<script setup lang="ts">
// 受保护页占位视图（票 B4-T1 骨架）：12 页 IA 的实现归后续页面票，
// 本票以占位视图 + 页面标题（i18n）立住路由骨架与守卫语义。

import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'

const route = useRoute()
const { t } = useI18n()

// 页面标题经 i18n key（routes.*）解析；key 缺失回落路由路径。
const titleKey = typeof route.meta.title === 'string' ? route.meta.title : null
const title = titleKey ? t(titleKey) : route.path
</script>

<template>
  <div class="placeholder-view">
    <h1>{{ title }}</h1>
    <p>{{ t('app.pageUnderConstruction') }}</p>
    <p class="placeholder-path">{{ route.path }}</p>
  </div>
</template>
