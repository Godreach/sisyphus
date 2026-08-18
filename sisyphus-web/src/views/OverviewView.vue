<script setup lang="ts">
// 概览页（ADR-0019/0020，票 B4-T3）：stat 卡 + 事实型警示态 + 最近构建。
//
// 数据源（Spec B4 决策 2 + 票 B4-T3）：概览快照端点（内部快照 / /metrics）
// 尚未交付，本页以现有端点组合派生当前值，并对不可派生的统计显式标注退化：
// - Agent 在线/总数：`/agents`（全局 admin 专属；普通用户 403 → 卡片以
//   「仅全局管理员可见」退化展示）。
// - 项目数：`/projects`（可见性过滤）。
// - 队列深度 / 构建终态计数 / 全局最近构建 / 无匹配任务 / 排空 / 不兼容
//   Agent：依赖概览快照端点或 pipeline 枚举端点，未交付 → 显式标注退化
//   （`overview.degraded`），后端补票后在原卡片接上，不静默给假值。
// - 警示态只展示「事实」：有 Agent 离线（ADR-0019 事实型警示，零阈值）；
//   无匹配任务 / 排空 / 不兼容 需快照端点，随退化面标注。

import { onMounted } from 'vue'
import { useI18n } from 'vue-i18n'

import { useOverviewStore } from '@/stores/overview'

const { t } = useI18n()
const overview = useOverviewStore()

onMounted(() => {
  void overview.load()
})
</script>

<template>
  <div class="overview-page">
    <h1 class="page-title">{{ t('routes.overview') }}</h1>

    <p v-if="overview.loadError" class="overview-error" role="alert">
      {{ overview.loadError }}
    </p>

    <!-- stat 卡（ADR-0019：只展示当前值，无历史曲线）。 -->
    <section class="stat-grid" aria-label="stat cards">
      <!-- Agent 在线/总数：/agents（全局 admin 专属）。 -->
      <div class="stat-card">
        <span class="stat-label">{{ t('overview.agentsOnline') }}</span>
        <span class="stat-value">
          <template v-if="overview.agents?.visible">
            {{ overview.agents.online }} / {{ overview.agents.total }}
          </template>
          <template v-else-if="overview.agents == null">
            {{ t('overview.na') }}
          </template>
          <template v-else>
            {{ t('overview.adminOnly') }}
          </template>
        </span>
      </div>

      <!-- 项目数：/projects（可见性过滤）。 -->
      <div class="stat-card">
        <span class="stat-label">{{ t('overview.projects') }}</span>
        <span class="stat-value">
          <template v-if="overview.projectCount != null">{{ overview.projectCount }}</template>
          <template v-else>{{ t('overview.na') }}</template>
        </span>
      </div>

      <!-- 队列深度 / 构建终态计数：依赖概览快照端点，未交付 → 退化。 -->
      <div class="stat-card stat-degraded">
        <span class="stat-label">{{ t('overview.queueDepth') }}</span>
        <span class="stat-value">{{ t('overview.degradedPending') }}</span>
        <span class="stat-degraded-note">{{ t('overview.degradedNote') }}</span>
      </div>
      <div class="stat-card stat-degraded">
        <span class="stat-label">{{ t('overview.buildOutcomes') }}</span>
        <span class="stat-value">{{ t('overview.degradedPending') }}</span>
        <span class="stat-degraded-note">{{ t('overview.degradedNote') }}</span>
      </div>
    </section>

    <!-- 事实型警示态（ADR-0019：事实判断、零阈值配置）。 -->
    <section class="alerts" aria-label="alerts">
      <p v-if="overview.offlineAlert" class="alert alert-warn" role="alert">
        {{ t('overview.alertAgentsOffline') }}
      </p>
      <p v-if="overview.agents?.visible && overview.agents.total === 0" class="alert alert-info">
        {{ t('overview.alertNoAgents') }}
      </p>
      <p class="alert alert-degraded">
        {{ t('overview.alertsDegradedPrefix') }}：{{ t('overview.alertNoMatch') }}、
        {{ t('overview.alertDraining') }}、{{ t('overview.alertIncompatible') }}
      </p>
    </section>

    <!-- 最近构建：依赖全局构建列表端点，未交付 → 显式退化。 -->
    <section class="recent-builds">
      <h2>{{ t('overview.recentBuilds') }}</h2>
      <p class="recent-builds-degraded">{{ t('overview.recentBuildsDegraded') }}</p>
    </section>
  </div>
</template>
