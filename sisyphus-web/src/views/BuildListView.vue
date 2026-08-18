<script setup lang="ts">
// 构建列表页（票 B4-T4，ADR-0006）：`GET .../builds` 分页 + 状态过滤，
// 作为构建详情页（#66 本票）的入口——从列表行点击进入构建详情。
//
// - 分页：page/limit（默认 20，1..=100），倒序；状态过滤下拉（全部 +
//   queued/running/succeeded/failed/cancelled/timeout）。
// - 行展示：构建号、状态、触发源、触发人、attempt、开始/终态、耗时。
// - 加载失败（404 = 项目/pipeline 不可见）与空列表都有明确文案。

import { computed, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { buildsApi } from '@/api/client'
import { describeActionError } from '@/api/errors'
import { formatDateTime } from '@/utils/format'
import type { BuildListResponse, BuildStatusDto } from '@/api/types'

const route = useRoute()
const router = useRouter()
const { t } = useI18n()

const project = computed(() => String(route.params.name ?? ''))
const pipeline = computed(() => String(route.params.pipeline ?? ''))

const PAGE_SIZE = 20

const list = ref<BuildListResponse | null>(null)
const loading = ref(true)
const errorMessage = ref('')
const currentPage = ref(1)
const statusFilter = ref<BuildStatusDto | ''>('')

const totalPages = computed(() =>
  list.value ? Math.max(1, Math.ceil(list.value.total / list.value.limit)) : 1,
)

async function loadList(): Promise<void> {
  loading.value = true
  errorMessage.value = ''
  try {
    list.value = await buildsApi.list(project.value, pipeline.value, {
      page: currentPage.value,
      limit: PAGE_SIZE,
      status: statusFilter.value,
    })
  } catch (err) {
    // 统一错误描述（网络失败/泛化与详情页动作共用同一分支，errors.ts）。
    errorMessage.value = describeActionError(err)
    list.value = null
  } finally {
    loading.value = false
  }
}

function goPage(page: number): void {
  if (page < 1 || page > totalPages.value) return
  currentPage.value = page
  void loadList()
}

function changeStatus(): void {
  currentPage.value = 1
  void loadList()
}

function openBuild(number: number): void {
  void router.push({
    name: 'build-detail',
    params: { name: project.value, pipeline: pipeline.value, number: String(number) },
  })
}

function buildStatusKey(status: BuildStatusDto): string {
  return `buildStatus.${status}`
}

function triggerKey(trigger: string): string {
  return `triggerSource.${trigger}`
}

onMounted(loadList)

watch(
  () => [project.value, pipeline.value],
  () => {
    currentPage.value = 1
    statusFilter.value = ''
    void loadList()
  },
)
</script>

<template>
  <div class="build-list-page">
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link :to="{ name: 'project-detail', params: { name: project } }">
        {{ project }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ pipeline }}</span>
    </nav>

    <header class="build-list-header">
      <h1>{{ pipeline }}</h1>
      <router-link
        :to="{ name: 'pipeline-edit', params: { name: project, pipeline } }"
        class="build-list-edit"
      >
        {{ t('buildList.editPipeline') }}
      </router-link>
    </header>

    <div class="build-list-toolbar">
      <label class="build-status-filter">
        <span>{{ t('buildList.filterByStatus') }}</span>
        <select v-model="statusFilter" @change="changeStatus" name="status-filter">
          <option value="">{{ t('buildList.allStatuses') }}</option>
          <option
            v-for="s in ['queued', 'running', 'succeeded', 'failed', 'cancelled', 'timeout']"
            :key="s"
            :value="s"
          >
            {{ t(buildStatusKey(s as BuildStatusDto)) }}
          </option>
        </select>
      </label>
    </div>

    <p v-if="loading" class="build-muted">{{ t('buildList.loading') }}</p>

    <p v-else-if="errorMessage" class="build-error" role="alert">
      {{ errorMessage }}
    </p>

    <p v-else-if="!list || list.items.length === 0" class="build-muted">
      {{ t('buildList.empty') }}
    </p>

    <table v-else class="build-list-table">
      <thead>
        <tr>
          <th>{{ t('buildList.number') }}</th>
          <th>{{ t('buildList.status') }}</th>
          <th>{{ t('buildList.trigger') }}</th>
          <th>{{ t('buildList.triggerBy') }}</th>
          <th>{{ t('buildList.attempt') }}</th>
          <th>{{ t('buildList.startedAt') }}</th>
          <th>{{ t('buildList.finishedAt') }}</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="item in list.items"
          :key="item.number"
          class="build-list-row"
          @click="openBuild(item.number)"
        >
          <td class="build-list-number">#{{ item.number }}</td>
          <td>
            <span class="build-status-badge" :class="`status-${item.status}`">
              {{ t(buildStatusKey(item.status)) }}
            </span>
          </td>
          <td>{{ t(triggerKey(item.trigger)) }}</td>
          <td>{{ item.trigger_by }}</td>
          <td>{{ item.attempt }}</td>
          <td>{{ formatDateTime(item.started_at) }}</td>
          <td>{{ formatDateTime(item.finished_at) }}</td>
        </tr>
      </tbody>
    </table>

    <div v-if="list && list.items.length > 0" class="build-list-pagination">
      <button
        type="button"
        class="btn"
        :disabled="currentPage <= 1"
        @click="goPage(currentPage - 1)"
      >
        {{ t('buildList.prev') }}
      </button>
      <span class="build-list-page-num">
        {{ t('buildList.page', { page: currentPage, total: totalPages }) }}
      </span>
      <button
        type="button"
        class="btn"
        :disabled="currentPage >= totalPages"
        @click="goPage(currentPage + 1)"
      >
        {{ t('buildList.next') }}
      </button>
    </div>
  </div>
</template>
