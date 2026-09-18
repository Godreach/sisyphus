<script setup lang="ts">
// 一级制品库（票 #127，ADR-0026）：状态、筛选与按来源聚合浏览。

import { computed, h, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  NAlert, NButton, NDataTable, NEmpty, NInput, NIcon, NPagination, NResult,
  NSelect, NSkeleton, NTag, type DataTableColumns, useMessage,
} from 'naive-ui'
import { RefreshOutline } from '@vicons/ionicons5'

import { artifactDeletionsApi, artifactRepositoryApi, projectsApi, s3ConfigApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type {
  ArtifactRepositoryItem, ArtifactRepositoryResponse, ArtifactRepositoryStatus,
  DeletionJobResponse, ProjectResponse, S3TestReportDto,
} from '@/api/types'
import { useAuthStore } from '@/stores/auth'

const { t } = useI18n()
const message = useMessage()
const auth = useAuthStore()
const isAdmin = computed(() => auth.user?.isAdmin === true)
const loading = ref(true)
const errorMessage = ref('')
const status = ref<ArtifactRepositoryStatus | null>(null)
const repository = ref<ArtifactRepositoryResponse | null>(null)
const repositoryError = ref('')
const page = ref(1)
const limit = 50
const filters = ref({ project: '', pipeline: '', build: '', job: '', attempt: '', name: '' })
const testing = ref(false)
const report = ref<S3TestReportDto | null>(null)
const testError = ref('')
const managedProjects = ref<ProjectResponse[]>([])
const selectedProject = ref<string | null>(null)
const deletionJobs = ref<DeletionJobResponse[]>([])
const deletionError = ref('')
const deletionLoading = ref(false)
const retryingDeletionId = ref<number | null>(null)

onMounted(() => { void loadStatus() })

async function loadStatus(): Promise<void> {
  loading.value = true
  errorMessage.value = ''
  try {
    status.value = await artifactRepositoryApi.status()
    if (status.value.available) {
      await Promise.all([loadItems(), loadDeletionManagement()])
    }
  } catch (err) {
    errorMessage.value = describeSubmitError(err)
    status.value = null
  } finally { loading.value = false }
}

function filterQuery(): Parameters<typeof artifactRepositoryApi.list>[0] {
  const query: Parameters<typeof artifactRepositoryApi.list>[0] = {
    project: filters.value.project || undefined,
    pipeline: filters.value.pipeline || undefined,
    job: filters.value.job || undefined,
    name: filters.value.name || undefined,
    page: page.value,
    limit,
  }
  const build = Number(filters.value.build)
  const attempt = Number(filters.value.attempt)
  if (Number.isInteger(build) && build > 0) query.build = build
  if (Number.isInteger(attempt) && attempt > 0) query.attempt = attempt
  return query
}

async function loadItems(): Promise<void> {
  repositoryError.value = ''
  try {
    const result = await artifactRepositoryApi.list(filterQuery())
    // 票 #122 的旧 mock 只返回 status；把它视为空列表，兼容开发期契约。
    repository.value = result?.items == null
      ? { items: [], total: 0, page: page.value, limit, legacy_local_count: 0 }
      : result
  } catch (err) {
    repositoryError.value = describeSubmitError(err)
    repository.value = null
  }
}

function applyFilters(): void { page.value = 1; void loadItems() }
function changePage(next: number): void { page.value = next; void loadItems() }
function formatSize(size: number): string {
  if (size < 1024) return `${size} B`
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KiB`
  if (size < 1024 * 1024 * 1024) return `${(size / (1024 * 1024)).toFixed(1)} MiB`
  return `${(size / (1024 * 1024 * 1024)).toFixed(2)} GiB`
}

const columns = computed<DataTableColumns<ArtifactRepositoryItem>>(() => [
  {
    title: t('artifacts.item'), key: 'name',
    render: (row) => h('div', { class: 'artifact-item-name' }, [
      h(NTag, { size: 'small', bordered: false, type: row.kind === 'set_entry' ? 'info' : 'default' }, {
        default: () => row.kind === 'set_entry' ? t('artifacts.setEntry') : t('artifacts.file'),
      }),
      h('span', row.set_name ? `${row.set_name}/${row.name}` : row.name),
    ]),
  },
  {
    title: t('artifacts.source'), key: 'source',
    render: (row) => `${row.source.project} / ${row.source.pipeline} / #${row.source.build}${row.source.job ? ` / ${row.source.job}` : ''}${row.source.attempt ? ` / a${row.source.attempt}` : ''}`,
  },
  { title: t('artifacts.size'), key: 'size', render: (row) => formatSize(row.size) },
  { title: t('artifacts.availability'), key: 'availability', render: (row) => h(NTag, { size: 'small', type: row.availability === 'ready' ? 'success' : 'warning' }, { default: () => t(`artifacts.availability_${row.availability}`, row.availability) }) },
  { title: t('artifacts.sha256'), key: 'sha256', render: (row) => h('code', { class: 'artifact-sha' }, row.sha256 || '—') },
  {
    title: t('artifacts.action'), key: 'action',
    render: (row) => row.download_url
      ? h('a', { class: 'artifact-download', href: row.download_url }, t('artifacts.download'))
      : h('span', { class: 'artifact-no-download' }, t('artifacts.directory')),
  },
])
const totalPages = computed(() => repository.value ? Math.max(1, Math.ceil(repository.value.total / limit)) : 1)

async function loadDeletionManagement(): Promise<void> {
  try {
    const result = await projectsApi.list({ permission: 'admin' })
    managedProjects.value = Array.isArray(result) ? result : []
    const first = managedProjects.value[0]?.name ?? null
    selectedProject.value = first
    if (first) await loadDeletionJobs(first)
  } catch {
    managedProjects.value = []
    selectedProject.value = null
  }
}

async function loadDeletionJobs(project = selectedProject.value): Promise<void> {
  if (!project) return
  deletionLoading.value = true
  deletionError.value = ''
  try {
    deletionJobs.value = (await artifactDeletionsApi.list(project)).items
  } catch (err) {
    deletionJobs.value = []
    deletionError.value = describeSubmitError(err)
  } finally {
    deletionLoading.value = false
  }
}

function selectProject(project: string): void {
  selectedProject.value = project
  void loadDeletionJobs(project)
}

async function retryDeletion(job: DeletionJobResponse): Promise<void> {
  if (!selectedProject.value) return
  retryingDeletionId.value = job.id
  try {
    const updated = await artifactDeletionsApi.retry(selectedProject.value, job.id)
    deletionJobs.value = deletionJobs.value.map((item) => item.id === updated.id ? updated : item)
    message.success(t('artifacts.deletionRetryQueued'))
  } catch (err) {
    message.error(describeSubmitError(err))
  } finally {
    retryingDeletionId.value = null
  }
}

const canTest = computed(
  () => isAdmin.value && status.value?.available === true && !testing.value,
)

async function testConnection(): Promise<void> {
  testing.value = true; testError.value = ''; report.value = null
  try {
    const result = await s3ConfigApi.testConnection()
    report.value = result
    message[result.ok ? 'success' : 'error'](result.ok ? t('artifacts.testOk') : t('artifacts.testFail'))
  } catch (err) {
    testError.value = describeSubmitError(err); message.error(testError.value)
  } finally { testing.value = false }
}
</script>

<template>
  <div class="artifact-repo-page" data-testid="artifact-repo-page">
    <n-skeleton v-if="loading" data-testid="artifact-repo-skeleton" text :repeat="4" />
    <n-alert v-else-if="errorMessage" type="error" data-testid="artifact-repo-error" :title="t('artifacts.loadError')">
      <p>{{ errorMessage }}</p>
      <n-button size="small" @click="loadStatus"><template #icon><n-icon :component="RefreshOutline" /></template>{{ t('artifacts.retry') }}</n-button>
    </n-alert>
    <n-result v-else-if="status && !status.available" status="warning" data-testid="artifact-repo-unavailable" :title="t('artifacts.unavailableTitle')" :description="t('artifacts.unavailableDesc')" />
    <div v-else-if="status?.available" class="artifact-repo-ready" data-testid="artifact-repo-available">
      <header class="artifact-repo-header">
        <div><h2 class="artifact-repo-title">{{ t('artifacts.availableTitle') }}</h2><p class="artifact-repo-desc">{{ t('artifacts.availableDesc') }}</p></div>
        <n-button v-if="isAdmin" type="primary" data-testid="artifact-repo-test" :loading="testing" :disabled="!canTest" @click="testConnection">{{ t('artifacts.testConnection') }}</n-button>
      </header>
      <dl v-if="status.backend" class="artifact-repo-backend">
        <div><dt>{{ t('artifacts.endpoint') }}</dt><dd>{{ status.backend.endpoint }}</dd></div>
        <div><dt>{{ t('artifacts.region') }}</dt><dd>{{ status.backend.region }}</dd></div>
        <div><dt>{{ t('artifacts.bucket') }}</dt><dd>{{ status.backend.bucket }}</dd></div>
        <div><dt>{{ t('artifacts.prefix') }}</dt><dd>{{ status.backend.prefix || t('artifacts.prefixEmpty') }}</dd></div>
      </dl>
      <n-alert v-if="repository && repository.legacy_local_count > 0" type="warning" data-testid="artifact-repo-legacy">{{ t('artifacts.legacyHint', { n: repository.legacy_local_count }) }}</n-alert>
      <n-alert v-if="testError" type="error" style="margin-top: 16px">{{ testError }}</n-alert>
      <ul v-if="report" class="artifact-repo-checks" data-testid="artifact-repo-report"><li v-for="check in report.checks" :key="check.op">{{ check.op }}: {{ check.ok ? t('artifacts.checkOk') : t('artifacts.checkFail') }}<span v-if="check.detail"> — {{ check.detail }}</span></li></ul>
      <section class="artifact-browser" data-testid="artifact-browser">
        <div class="artifact-filters">
          <n-input v-model:value="filters.project" :placeholder="t('artifacts.projectFilter')" data-testid="artifact-filter-project" />
          <n-input v-model:value="filters.pipeline" :placeholder="t('artifacts.pipelineFilter')" data-testid="artifact-filter-pipeline" />
          <n-input v-model:value="filters.build" :placeholder="t('artifacts.buildFilter')" data-testid="artifact-filter-build" />
          <n-input v-model:value="filters.job" :placeholder="t('artifacts.jobFilter')" data-testid="artifact-filter-job" />
          <n-input v-model:value="filters.attempt" :placeholder="t('artifacts.attemptFilter')" data-testid="artifact-filter-attempt" />
          <n-input v-model:value="filters.name" :placeholder="t('artifacts.nameFilter')" data-testid="artifact-filter-name" />
          <n-button type="primary" @click="applyFilters">{{ t('artifacts.filter') }}</n-button>
        </div>
        <n-alert v-if="repositoryError" type="error" data-testid="artifact-repo-list-error">{{ repositoryError }}</n-alert>
        <n-empty v-else-if="repository && repository.items.length === 0" :description="t('artifacts.empty')" />
        <n-data-table v-else-if="repository" :columns="columns" :data="repository.items" :bordered="false" :single-line="false" />
        <n-pagination v-if="repository && repository.total > limit" :page="page" :page-size="limit" :page-count="totalPages" @update:page="changePage" />
      </section>
      <section v-if="managedProjects.length > 0" class="deletion-panel" data-testid="deletion-panel">
        <div class="deletion-panel-head">
          <h3>{{ t('artifacts.deletionsTitle') }}</h3>
          <n-select
            :value="selectedProject"
            :options="managedProjects.map((project) => ({ label: project.name, value: project.name }))"
            :virtual-scroll="false"
            style="width: 220px"
            @update:value="selectProject"
          />
        </div>
        <n-alert v-if="deletionError" type="error">{{ deletionError }}</n-alert>
        <n-skeleton v-else-if="deletionLoading" text :repeat="2" />
        <n-empty v-else-if="deletionJobs.length === 0" :description="t('artifacts.deletionsEmpty')" />
        <ul v-else class="deletion-jobs">
          <li v-for="job in deletionJobs" :key="job.id" :data-testid="`deletion-job-${job.id}`">
            <div>
              <strong>{{ t(`artifacts.deletionScope.${job.scope}`) }}</strong>
              <span v-if="job.pipeline_name"> {{ job.pipeline_name }} #{{ job.build_number }}</span>
              <span v-if="job.set_id"> · {{ t('artifacts.deletionScope.set') }} {{ job.set_id }}</span>
            </div>
            <n-tag :type="job.state === 'failed' ? 'error' : job.state === 'completed' ? 'success' : 'info'">
              {{ t(`artifacts.deletionState.${job.state}`) }}
            </n-tag>
            <span>{{ t('artifacts.deletionAttempts', { count: job.attempts }) }}</span>
            <span v-if="job.last_error" class="deletion-error">{{ job.last_error }}</span>
            <n-button
              v-if="job.state === 'failed'"
              size="small"
              :loading="retryingDeletionId === job.id"
              :data-testid="`retry-deletion-${job.id}`"
              @click="retryDeletion(job)"
            >
              {{ t('artifacts.retry') }}
            </n-button>
          </li>
        </ul>
      </section>
    </div>
    <n-empty v-else :description="t('artifacts.loadError')" />
  </div>
</template>

<style scoped>
.artifact-repo-page { padding: 24px; max-width: 1320px; }
.artifact-repo-header { display: flex; justify-content: space-between; align-items: flex-start; gap: 16px; }
.artifact-repo-title { margin: 0 0 8px; font-size: 20px; font-weight: 600; }
.artifact-repo-desc { margin: 0 0 16px; color: var(--n-text-color-3, #86868b); }
.artifact-repo-backend { display: grid; gap: 8px 24px; margin: 0 0 20px; grid-template-columns: auto 1fr; }
.artifact-repo-backend > div { display: contents; }
.artifact-repo-backend dt { color: var(--n-text-color-3, #86868b); }
.artifact-repo-backend dd { margin: 0; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
.artifact-repo-checks { margin: 16px 0 0; padding-left: 20px; }
.artifact-browser { margin-top: 24px; }
.artifact-filters { display: grid; grid-template-columns: repeat(3, minmax(140px, 1fr)) auto; gap: 10px; margin-bottom: 16px; }
.artifact-item-name { display: flex; align-items: center; gap: 8px; }
.artifact-sha { font-size: 11px; }
.artifact-download { color: var(--n-color-target, #18a058); }
@media (max-width: 900px) { .artifact-filters { grid-template-columns: repeat(2, minmax(140px, 1fr)); } }
.deletion-panel {
  margin-top: 28px;
  border-top: 1px solid var(--n-border-color, #e5e7eb);
  padding-top: 20px;
}
.deletion-panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  margin-bottom: 12px;
}
.deletion-panel-head h3 { margin: 0; }
.deletion-jobs {
  display: grid;
  gap: 10px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.deletion-jobs li {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 10px;
  padding: 12px;
  border: 1px solid var(--n-border-color, #e5e7eb);
  border-radius: 8px;
}
.deletion-error { color: #d03050; }
</style>
