<script setup lang="ts">
// 概览页（ADR-0019，票 B5-T7）：stat 卡 + 事实型警示态 + 最近构建。
//
// 数据源（票 B5-T7 交付后单一来源）：概览快照端点 `GET /api/v1/overview`——
// 队列深度（原因分类）/ Agent 在线与总数 / 槽位占用 / 构建终态计数 / 产物与
// 日志占用 / 三类事实警示态 / 最近构建，任意登录角色可读。B4-T3 的退化标注
// （依赖快照端点未交付）随本票移除。
//
// - 单来源整页语义：快照失败 → loadError 报错 + 重试，不静默部分值。
// - 警示态只展示「事实」（ADR-0019：零阈值）：无匹配任务 / 有离线 Agent /
//   排空或不兼容 Agent——全部来自快照响应的 alerts 字段。
// - 时间 / 字节数人读形态复用 `@/utils/format`（与构建列表/详情同纪律）。

import { onMounted } from 'vue'
import { useI18n } from 'vue-i18n'

import { useOverviewStore } from '@/stores/overview'
import { formatBytes, formatDateTime } from '@/utils/format'

const { t } = useI18n()
const overview = useOverviewStore()

onMounted(() => {
  void overview.load()
})

/** 队列原因 → 人读标签键（与后端 snapshot::classify 固定标签全集对应）。 */
function queueReasonKey(reason: string): string {
  return `overview.queueReason.${reason}`
}

/** 构建状态 → 人读标签键（复用 buildStatus.*，与构建列表/详情同纪律）。 */
function buildStatusKey(status: string): string {
  return `buildStatus.${status}`
}

/** 触发源 → 人读标签键（复用 triggerSource.*）。 */
function triggerKey(trigger: string): string {
  return `triggerSource.${trigger}`
}
</script>

<template>
  <div class="overview-page">
    <h1 class="page-title">{{ t('routes.overview') }}</h1>

    <p v-if="overview.loadError" class="overview-error" role="alert">
      {{ overview.loadError }}
      <button type="button" @click="overview.load()">{{ t('overview.retry') }}</button>
    </p>

    <template v-if="overview.state != null">
      <!-- stat 卡（ADR-0019：只展示当前值，无历史曲线）。 -->
      <section class="stat-grid" aria-label="stat cards">
        <div class="stat-card">
          <span class="stat-label">{{ t('overview.agentsOnline') }}</span>
          <span class="stat-value">
            {{ overview.state.agentsOnline }} / {{ overview.state.agentsTotal }}
          </span>
        </div>
        <div class="stat-card">
          <span class="stat-label">{{ t('overview.slots') }}</span>
          <span class="stat-value">
            {{ overview.state.slotsUsed }} / {{ overview.state.slotsTotal }}
          </span>
        </div>
        <div class="stat-card">
          <span class="stat-label">{{ t('overview.queueDepth') }}</span>
          <span class="stat-value">{{ overview.state.queueDepth }}</span>
        </div>
        <div class="stat-card">
          <span class="stat-label">{{ t('overview.buildOutcomes') }}</span>
          <span class="stat-value">
            <span class="build-outcome build-outcome-ok">
              {{ t('overview.outcomeSucceeded') }} {{ overview.state.buildsTerminal.succeeded }}
            </span>
            <span class="build-outcome build-outcome-fail">
              {{ t('overview.outcomeFailed') }} {{ overview.state.buildsTerminal.failed }}
            </span>
            <span class="build-outcome">
              {{ t('overview.outcomeCancelled') }} {{ overview.state.buildsTerminal.cancelled }}
            </span>
            <span class="build-outcome">
              {{ t('overview.outcomeTimeout') }} {{ overview.state.buildsTerminal.timeout }}
            </span>
          </span>
        </div>
        <div class="stat-card">
          <span class="stat-label">{{ t('overview.storage') }}</span>
          <span class="stat-value">
            {{ formatBytes(overview.state.artifactBytes) }} +
            {{ formatBytes(overview.state.logBytes) }}
          </span>
        </div>
      </section>

      <!-- 队列原因分类（有任务等待时给出「卡在哪」的事实）。 -->
      <section v-if="overview.state.queueReasons.length > 0" class="queue-reasons">
        <h2>{{ t('overview.queueReasons') }}</h2>
        <ul class="queue-reason-list">
          <li v-for="r in overview.state.queueReasons" :key="r.reason">
            <span class="queue-reason-label">{{ t(queueReasonKey(r.reason)) }}</span>
            <span class="queue-reason-depth">{{ r.depth }}</span>
          </li>
        </ul>
      </section>

      <!-- 事实型警示态（ADR-0019：事实判断、零阈值配置）。 -->
      <section class="alerts" aria-label="alerts">
        <p v-if="overview.offlineAlert" class="alert alert-warn" role="alert">
          {{ t('overview.alertAgentsOffline') }}
        </p>
        <p v-if="overview.noMatchAlert" class="alert alert-warn" role="alert">
          {{ t('overview.alertNoMatch') }}
        </p>
        <p v-if="overview.drainingIncompatibleAlert" class="alert alert-warn" role="alert">
          {{ t('overview.alertDrainingIncompatible') }}
        </p>
        <p v-if="overview.state.agentsTotal === 0" class="alert alert-info">
          {{ t('overview.alertNoAgents') }}
        </p>
      </section>

      <!-- 最近构建（快照响应，跨可见项目按最近活动倒序）。 -->
      <section class="recent-builds">
        <h2>{{ t('overview.recentBuilds') }}</h2>
        <div v-if="overview.state.recentBuilds.length === 0" class="recent-builds-empty">
          {{ t('overview.recentBuildsEmpty') }}
        </div>
        <table v-else class="recent-builds-table">
          <thead>
            <tr>
              <th>{{ t('overview.colProject') }}</th>
              <th>{{ t('overview.colPipeline') }}</th>
              <th>{{ t('overview.colBuild') }}</th>
              <th>{{ t('overview.colStatus') }}</th>
              <th>{{ t('overview.colTrigger') }}</th>
              <th>{{ t('overview.colFinished') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="b in overview.state.recentBuilds" :key="`${b.project}-${b.pipeline}-${b.number}`">
              <td>{{ b.project }}</td>
              <td>{{ b.pipeline }}</td>
              <td>#{{ b.number }}</td>
              <td>{{ t(buildStatusKey(b.status)) }}</td>
              <td>{{ t(triggerKey(b.trigger)) }}</td>
              <td>{{ formatDateTime(b.finishedAt ?? b.startedAt) }}</td>
            </tr>
          </tbody>
        </table>
      </section>
    </template>
  </div>
</template>