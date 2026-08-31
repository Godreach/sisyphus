<script setup lang="ts">
// 项目详情页（票 #108，spec #100 定稿铺开；ADR-0014/0016/0020）。
//
// 视觉：以定稿三主页面为推导源，复用共享组件类——badge 胶囊徽章、sisy-card、
// btn-outline 描边动作、breadcrumb 面包屑；NTabs 碎片化废止，五张卡纵向堆叠
// （项目信息 / 流水线 / 最近构建 / 成员角色 / SCM 凭据）。
//
// 数据：
// - 项目元数据 `GET /projects/{name}`（viewer 档声明：无角色与不存在同形 404，
//   B2b-T5）——页面锚点：404 整页退化态、500 整页报错 + 重试、首载骨架屏。
// - 流水线区：清单 `GET /pipelines`（契约票 #105）按项目过滤 + 逐条
//   `GET …/stats?window=20`（契约票 #102）——真清单，不再有探测退化。清单
//   失败卡内报错 + 重试（局部失败不整页化）；行内动作（终止/重试/运行）与
//   流水线页共用 utils/pipelineAction 映射；「编辑」跳混合编辑器、
//   「新建流水线」弹窗输名跳编辑器（404 → 空定义保存即创建，编辑器既有语义）。
// - 最近构建：逐条流水线 `GET …/builds?limit=8` 客户端合并取最近 8 条
//   （无新契约）；行点击跳构建详情。
// - 轻轮询（5s）：仅对最近构建为排队/运行中的流水线重取统计并重刷最近
//   构建合并——mock 动态构建生命周期在页面「活」起来（PipelinesView 同口径）。
// - 成员/SCM 凭据（项目 admin 档）：403 卡内退化提示（membersAdminOnly /
//   credentialAdminOnly），不渲染表单；成员保存为整组替换（PUT）。
// - 编辑项目（契约先行，`PATCH /projects/{name}`，项目 admin 档）：scm_url /
//   default_branch 可选字段，本票即契约冻结点，后端阶段照单实现。

import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NEmpty,
  NFormItem,
  NInput,
  NModal,
  NSelect,
  NSkeleton,
  useMessage,
} from 'naive-ui'

import { buildsApi, pipelinesApi, projectsApi, usersApi } from '@/api/client'
import { describeActionError, describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import {
  formatDateTime,
  formatDuration,
  relativeAge,
  relativeAgeKey,
  statusBadgeClass,
} from '@/utils/format'
import {
  pipelineRowActionFor,
  runPipelineRowAction,
  type PipelineRowAction,
} from '@/utils/pipelineAction'
import type {
  BuildSummaryResponse,
  LatestBuildRef,
  MemberAssignment,
  MemberResponse,
  MemberRoleDto,
  ProjectResponse,
} from '@/api/types'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const message = useMessage()

const projectName = computed(() => String(route.params.name ?? ''))

/** 组件卸载后异步续跑不得再发请求/写状态——卸载后的 route 被 vue-router
 *  复位（测试环境实测回 '/'），projectName 不再可采信。 */
let disposed = false

/** 统计窗口（契约票 #102；与流水线页同口径）。 */
const WINDOW = 20
/** 最近构建每条流水线取回条数（合并后全局取 8）。 */
const RECENT_PER_PIPELINE = 8
/** 活跃行轻轮询间隔。 */
const POLL_MS = 5000

// ---------------------------------------------------------------------------
// 项目元数据（页面锚点）
// ---------------------------------------------------------------------------

const project = ref<ProjectResponse | null>(null)
const loading = ref(true)
const loadError = ref('')
const notFound = ref(false)

async function loadProject(): Promise<void> {
  loading.value = true
  loadError.value = ''
  notFound.value = false
  try {
    project.value = await projectsApi.get(projectName.value)
    void loadPipelines()
    void loadMembers()
  } catch (err) {
    project.value = null
    if (err instanceof ApiError && err.status === 404) {
      notFound.value = true
    } else {
      loadError.value = describeSubmitError(err)
    }
  } finally {
    loading.value = false
  }
}

onMounted(() => {
  void loadProject()
  pollTimer = setInterval(() => {
    void refreshLive()
  }, POLL_MS)
})

let pollTimer: ReturnType<typeof setInterval> | null = null

onBeforeUnmount(() => {
  disposed = true
  if (pollTimer != null) clearInterval(pollTimer)
  pollTimer = null
})

// ---------------------------------------------------------------------------
// 流水线区（清单过滤项目维 + stats；行内动作与流水线页同映射）
// ---------------------------------------------------------------------------

interface PipelineRow {
  pipeline: string
  /** 最近一条构建（stats 端点 latest_build，任意状态）；从未运行为 null。 */
  latest: LatestBuildRef | null
  /** 窗口内终态成功率（无终态 null → 「—」）。 */
  rate: string | null
  /** 窗口内终态平均耗时毫秒（无样本 null → 「—」）。 */
  avgMs: number | null
  /** 行内动作（随 latest 派生，加载时单点计算——渲染期零重算）。 */
  action: PipelineRowAction
}

const rows = ref<PipelineRow[]>([])
const pplLoading = ref(true)
const pplError = ref('')
/** 动作进行中（按行标记，按钮禁用）。 */
const actingPipeline = ref('')

async function loadPipelines(): Promise<void> {
  const name = projectName.value
  if (disposed || name === '') return
  pplLoading.value = true
  pplError.value = ''
  try {
    const list = await pipelinesApi.list()
    const mine = list.items.filter((item) => item.project === name)
    const loaded = await Promise.all(mine.map((item) => loadRow(name, item.pipeline)))
    rows.value = loaded
    void loadRecent()
  } catch (err) {
    rows.value = []
    pplError.value = describeSubmitError(err)
  } finally {
    pplLoading.value = false
  }
}

/** 单条流水线 → 行数据；统计不可见（404）或权限不足（403）按「未运行」行展示。 */
async function loadRow(name: string, pipeline: string): Promise<PipelineRow> {
  try {
    const stats = await pipelinesApi.stats(name, pipeline, WINDOW)
    return {
      pipeline,
      latest: stats.latest_build,
      rate: stats.success_rate != null ? `${stats.success_rate}%` : null,
      avgMs: stats.avg_duration_ms,
      action: pipelineRowActionFor(stats.latest_build?.status),
    }
  } catch {
    return { pipeline, latest: null, rate: null, avgMs: null, action: pipelineRowActionFor(null) }
  }
}

function latestRunText(row: PipelineRow): string {
  if (!row.latest) return t('plines.noRun')
  const age = relativeAge(row.latest.finished_at ?? row.latest.started_at)
  return `#${row.latest.number} · ${t(relativeAgeKey(age), { n: age.n })}`
}

async function runRowAction(row: PipelineRow): Promise<void> {
  if (actingPipeline.value !== '') return
  actingPipeline.value = row.pipeline
  try {
    const toast = await runPipelineRowAction(row.action.kind, {
      project: projectName.value,
      pipeline: row.pipeline,
      latest: row.latest,
    })
    message.success(
      toast.key === 'plines.triggered' ? t(toast.key, toast.params) : t(toast.key),
    )
    // 动作落定后单行刷新 + 最近构建合并刷新（动态构建即时可见）。
    const fresh = await loadRow(projectName.value, row.pipeline)
    rows.value = rows.value.map((r) => (r.pipeline === row.pipeline ? fresh : r))
    void loadRecent()
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    actingPipeline.value = ''
  }
}

function openPipeline(pipeline: string): void {
  void router.push({ name: 'build-list', params: { name: projectName.value, pipeline } })
}

function openEditor(pipeline: string): void {
  void router.push({ name: 'pipeline-edit', params: { name: projectName.value, pipeline } })
}

// ---------------------------------------------------------------------------
// 最近构建（逐流水线构建列表客户端合并，取最近 8 条）
// ---------------------------------------------------------------------------

const runs = ref<BuildSummaryResponse[]>([])
const runsLoading = ref(true)
const runsError = ref('')

async function loadRecent(): Promise<void> {
  const name = projectName.value
  if (disposed || name === '') return
  const pipelines = rows.value.map((r) => r.pipeline)
  runsLoading.value = true
  runsError.value = ''
  try {
    const lists = await Promise.all(
      pipelines.map((p) =>
        buildsApi.list(name, p, { page: 1, limit: RECENT_PER_PIPELINE, status: '' }),
      ),
    )
    runs.value = lists
      .flatMap((l) => l.items)
      .sort((a, b) => (b.finished_at ?? b.started_at ?? 0) - (a.finished_at ?? a.started_at ?? 0))
      .slice(0, RECENT_PER_PIPELINE)
  } catch (err) {
    runs.value = []
    runsError.value = describeSubmitError(err)
  } finally {
    runsLoading.value = false
  }
}

function runDuration(row: BuildSummaryResponse): number | null {
  if (row.started_at == null) return null
  return Math.max(0, (row.finished_at ?? Date.now()) - row.started_at)
}

function relativeTimeText(ms: number | null): string {
  const age = relativeAge(ms)
  return t(relativeAgeKey(age), { n: age.n })
}

function openBuild(row: BuildSummaryResponse): void {
  void router.push({
    name: 'build-detail',
    params: { name: projectName.value, pipeline: row.pipeline_name, number: String(row.number) },
  })
}

/** 轻轮询：仅重取最近构建为排队/运行中的行（统计），并刷新最近构建合并。 */
async function refreshLive(): Promise<void> {
  const name = projectName.value
  if (disposed || name === '' || loading.value || pplLoading.value) return
  const live = rows.value.filter(
    (r) => r.latest?.status === 'queued' || r.latest?.status === 'running',
  )
  const liveRuns = runs.value.some((r) => r.status === 'queued' || r.status === 'running')
  if (live.length === 0 && !liveRuns) return
  if (live.length > 0) {
    const fresh = await Promise.all(live.map((r) => loadRow(name, r.pipeline)))
    // 统计取不到的行不回写（瞬时失败不把在跑行翻成「未运行」；下轮自修）。
    const freshByName = new Map(fresh.filter((r) => r.latest != null).map((r) => [r.pipeline, r]))
    if (freshByName.size > 0) {
      rows.value = rows.value.map((r) => freshByName.get(r.pipeline) ?? r)
    }
  }
  void loadRecent()
}

// ---------------------------------------------------------------------------
// 成员角色（项目 admin 档；403 卡内退化）
// ---------------------------------------------------------------------------

const members = ref<MemberResponse[] | null>(null)
const directory = ref<string[]>([])
const memberError = ref('')
/** 成员面首次加载落定（骨架 → 表单/403 提示；未落定前不闪 403 假提示）。 */
const membersDone = ref(false)
/** 成员面 403（非项目 admin）：凭据卡退化提示同源门控；其它错误不算 403。 */
const memberForbidden = ref(false)
/** 当前用户有项目 admin 档（成员清单可读 = 是）；凭据卡与编辑项目按钮同门控。 */
const isProjectAdmin = ref(false)
const newMember = ref('')
const newRole = ref<MemberRoleDto>('viewer')
const savingMembers = ref(false)

/** 目录中尚不是成员的用户（下拉选项——避免整组替换提交重名行）。 */
const addableUsers = computed(() =>
  directory.value.filter((u) => !members.value?.some((m) => m.username === u)),
)

async function loadMembers(): Promise<void> {
  memberError.value = ''
  membersDone.value = false
  memberForbidden.value = false
  isProjectAdmin.value = false
  try {
    const [memberRows, dir] = await Promise.all([
      projectsApi.listMembers(projectName.value),
      usersApi.directory(),
    ])
    members.value = memberRows
    directory.value = dir.map((d) => d.username)
    isProjectAdmin.value = true
  } catch (err) {
    members.value = null
    // 403 = 非项目 admin：卡内退化提示（非致命，viewer 面正常展示）；其它错误
    // 走错误展示（不伪装成权限提示）。
    if (err instanceof ApiError && err.status === 403) {
      memberForbidden.value = true
    } else {
      memberError.value = describeSubmitError(err)
    }
  } finally {
    membersDone.value = true
  }
}

/** 提交成员整组替换（PUT 语义：当前表单成员 = 完整清单，未列入者移除）。 */
async function saveMembers(): Promise<void> {
  if (!members.value) return
  memberError.value = ''
  savingMembers.value = true
  try {
    const assignments: MemberAssignment[] = members.value.map((m) => ({
      username: m.username,
      role: m.role,
    }))
    if (newMember.value) {
      assignments.push({ username: newMember.value, role: newRole.value })
    }
    members.value = await projectsApi.replaceMembers(projectName.value, assignments)
    newMember.value = ''
    message.success(t('projects.membersSaved'))
  } catch (err) {
    memberError.value = describeSubmitError(err)
  } finally {
    savingMembers.value = false
  }
}

/** 从表单移除一名成员（整组替换在保存时生效）。 */
function removeMember(username: string): void {
  if (members.value) {
    members.value = members.value.filter((m) => m.username !== username)
  }
}

const ROLE_OPTIONS: { label: MemberRoleDto; value: MemberRoleDto }[] = [
  { label: 'viewer', value: 'viewer' },
  { label: 'runner', value: 'runner' },
  { label: 'admin', value: 'admin' },
]

// ---------------------------------------------------------------------------
// SCM 凭据（项目 admin 档；username + password 皆空 = 清除）
// ---------------------------------------------------------------------------

const credUsername = ref('')
const credPassword = ref('')
const savingCred = ref(false)
const credError = ref('')
const testingCred = ref(false)
const credProbeState = ref<'success' | 'error' | null>(null)
const credProbeMsg = ref('')

async function saveCredential(): Promise<void> {
  credError.value = ''
  savingCred.value = true
  try {
    await projectsApi.putScmCredential(projectName.value, {
      username: credUsername.value.trim() || null,
      password: credPassword.value || null,
    })
    const cleared = credUsername.value.trim() === '' && credPassword.value === ''
    credUsername.value = ''
    credPassword.value = ''
    credProbeState.value = null
    credProbeMsg.value = ''
    message.success(cleared ? t('projects.credentialCleared') : t('projects.credentialSaved'))
  } catch (err) {
    credError.value = describeSubmitError(err)
  } finally {
    savingCred.value = false
  }
}

async function testCredential(): Promise<void> {
  credProbeState.value = null
  credProbeMsg.value = ''
  testingCred.value = true
  try {
    const probe = await projectsApi.testConnection(projectName.value)
    credProbeState.value = 'success'
    credProbeMsg.value =
      probe.head === null
        ? t('projects.testConnectionEmpty')
        : t('projects.testConnectionOk', { head: probe.head })
  } catch (err) {
    credProbeState.value = 'error'
    credProbeMsg.value = describeSubmitError(err)
  } finally {
    testingCred.value = false
  }
}

// ---------------------------------------------------------------------------
// 编辑项目（契约先行，票 #108，`PATCH /projects/{name}`，项目 admin 档）
// ---------------------------------------------------------------------------

const editOpen = ref(false)
const editUrl = ref('')
const editBranch = ref('')
const editError = ref('')
const editSaving = ref(false)

function openEditProject(): void {
  editUrl.value = project.value?.scm_url ?? ''
  editBranch.value = project.value?.default_branch ?? ''
  editError.value = ''
  editOpen.value = true
}

async function submitEditProject(): Promise<void> {
  editError.value = ''
  const url = editUrl.value.trim()
  if (url === '') {
    editError.value = t('projects.scmUrlRequired')
    return
  }
  if (!/^https?:\/\/.+/.test(url)) {
    editError.value = t('projects.scmUrlInvalid')
    return
  }
  editSaving.value = true
  try {
    const isGit = project.value?.scm_type === 'git'
    project.value = await projectsApi.update(projectName.value, {
      scm_url: url,
      default_branch: isGit ? editBranch.value.trim() || null : null,
    })
    editOpen.value = false
    message.success(t('projects.editSaved'))
  } catch (err) {
    editError.value = describeSubmitError(err)
  } finally {
    editSaving.value = false
  }
}

// ---------------------------------------------------------------------------
// 新建流水线（输名 → 跳编辑器；404 → 空定义保存即创建，编辑器既有语义）
// ---------------------------------------------------------------------------

const newOpen = ref(false)
const newPipelineName = ref('')
const newNameError = ref('')

function openNewPipeline(): void {
  newPipelineName.value = ''
  newNameError.value = ''
  newOpen.value = true
}

function createPipeline(): void {
  const name = newPipelineName.value.trim()
  if (name === '') {
    newNameError.value = t('projects.newPipelineNameRequired')
    return
  }
  newOpen.value = false
  openEditor(name)
}
</script>

<template>
  <!-- 首载骨架屏（事实态纪律——数据到达后替换）。 -->
  <div v-if="loading" class="project-detail-page" data-testid="project-detail-skeleton">
    <n-skeleton text :repeat="1" height="32px" class="pdl-skeleton-row" />
    <n-skeleton text :repeat="2" height="56px" class="pdl-skeleton-row" />
    <n-skeleton text :repeat="4" height="72px" class="pdl-skeleton-row" />
  </div>

  <!-- 404 同形（无角色与不存在同形，B2b-T5）：整页退化 + 返回项目列表。 -->
  <div v-else-if="notFound" class="project-detail-page">
    <n-alert
      type="error"
      :title="t('projects.notFound')"
      role="alert"
      data-testid="project-detail-notfound"
    />
    <router-link class="card-link" data-testid="project-detail-back" :to="{ name: 'projects' }">
      {{ t('buildDetail.backToProjects') }}
    </router-link>
  </div>

  <!-- 加载失败：整页报错 + 重试（事实态纪律，与主页面同形）。 -->
  <div v-else-if="loadError" class="project-detail-page">
    <n-alert type="error" :title="loadError" role="alert">
      <button type="button" class="btn-outline blue" data-testid="project-detail-retry" @click="loadProject">
        {{ t('plines.retry') }}
      </button>
    </n-alert>
  </div>

  <div v-else-if="project" class="project-detail-page">
    <!-- 面包屑：项目 / {name}（ADR-0020）。 -->
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ project.name }}</span>
    </nav>

    <!-- 页头：项目名 + scm 徽章 + 动作（编辑项目·项目 admin 档；新建流水线）。 -->
    <header class="page-header project-detail-header">
      <div class="project-title-row">
        <h1 class="page-title project-title" data-testid="project-title">{{ project.name }}</h1>
        <span class="badge neutral">{{ project.scm_type }}</span>
      </div>
      <div class="project-actions">
        <button
          v-if="isProjectAdmin"
          type="button"
          class="btn-outline"
          data-testid="edit-project-btn"
          @click="openEditProject"
        >
          {{ t('projects.editProject') }}
        </button>
        <button
          type="button"
          class="btn-outline blue"
          data-testid="new-pipeline-btn"
          @click="openNewPipeline"
        >
          {{ t('plines.newPipeline') }}
        </button>
      </div>
    </header>

    <!-- 项目信息卡（viewer 档元数据）。 -->
    <section class="sisy-card project-info-card" aria-label="project info">
      <div class="card-header">
        <h2 class="card-title">{{ t('projects.infoTitle') }}</h2>
      </div>
      <dl class="meta-grid">
        <div class="meta-item">
          <dt>{{ t('projects.scmType') }}</dt>
          <dd>{{ project.scm_type }}</dd>
        </div>
        <div class="meta-item">
          <dt>{{ t('projects.scmUrl') }}</dt>
          <dd class="mono-url">{{ project.scm_url }}</dd>
        </div>
        <div v-if="project.scm_type === 'git'" class="meta-item">
          <dt>{{ t('projects.metaDefaultBranch') }}</dt>
          <dd>{{ project.default_branch ?? '—' }}</dd>
        </div>
        <div class="meta-item">
          <dt>{{ t('projects.metaCreatedAt') }}</dt>
          <dd>{{ formatDateTime(project.created_at) }}</dd>
        </div>
      </dl>
    </section>

    <!-- 流水线卡：本项目流水线（真清单 + stats 行 + 行内动作 + 编辑）。 -->
    <section class="sisy-card project-pipelines-card" aria-label="project pipelines">
      <div class="card-header">
        <h2 class="card-title">{{ t('projects.pipelinesTitle') }}</h2>
      </div>

      <n-alert v-if="pplError" type="error" :title="pplError" role="alert" class="card-alert">
        <button type="button" class="btn-outline" data-testid="pipelines-retry" @click="loadPipelines">
          {{ t('plines.retry') }}
        </button>
      </n-alert>

      <div v-else-if="pplLoading" class="card-skeleton">
        <n-skeleton text :repeat="3" height="44px" />
      </div>

      <div v-else-if="rows.length === 0" class="ppl-empty" data-testid="pipelines-empty">
        <n-empty :description="t('projects.pipelinesEmpty')">
          <template #extra>
            <p class="form-hint">{{ t('projects.pipelinesEmptyHint') }}</p>
            <n-button
              type="primary"
              size="small"
              class="ppl-empty-btn"
              data-testid="pipeline-empty-create-btn"
              @click="openNewPipeline"
            >
              {{ t('plines.newPipeline') }}
            </n-button>
          </template>
        </n-empty>
      </div>

      <template v-else>
        <div class="ppl-thead">
          <span class="ppl-col-name">{{ t('plines.colPipeline') }}</span>
          <span class="ppl-col-status">{{ t('plines.colStatus') }}</span>
          <span class="ppl-col-rate">{{ t('plines.colRate') }}</span>
          <span class="ppl-col-avg">{{ t('plines.colAvg') }}</span>
          <span class="ppl-col-latest">{{ t('plines.latestRun') }}</span>
          <span class="ppl-col-action" />
        </div>
        <div
          v-for="row in rows"
          :key="row.pipeline"
          class="ppl-row"
          :data-testid="`pipeline-row-${row.pipeline}`"
        >
          <button type="button" class="ppl-name ppl-col-name" @click="openPipeline(row.pipeline)">
            <span class="n">{{ row.pipeline }}</span>
          </button>
          <div class="ppl-col-status">
            <span class="badge" :class="statusBadgeClass(row.latest?.status ?? '')">
              {{ row.latest ? t(`buildStatus.${row.latest.status}`) : t('plines.noRun') }}
            </span>
          </div>
          <span class="ppl-col-rate">{{ row.rate ?? '—' }}</span>
          <span class="ppl-col-avg">{{ row.avgMs != null ? formatDuration(row.avgMs) : '—' }}</span>
          <span class="ppl-col-latest">{{ latestRunText(row) }}</span>
          <div class="ppl-col-action">
            <button
              type="button"
              class="btn-outline"
              :class="row.action.cls"
              :disabled="actingPipeline === row.pipeline"
              :data-testid="`pipeline-action-${row.pipeline}`"
              @click="runRowAction(row)"
            >
              {{ t(row.action.labelKey) }}
            </button>
            <button
              type="button"
              class="btn-outline"
              :data-testid="`pipeline-edit-${row.pipeline}`"
              @click="openEditor(row.pipeline)"
            >
              {{ t('projects.pipelineEdit') }}
            </button>
          </div>
        </div>
      </template>
    </section>

    <!-- 最近构建卡（本项目全部流水线合并 top 8；行点击跳构建详情）。 -->
    <section class="sisy-card project-runs-card" aria-label="recent builds">
      <div class="card-header">
        <h2 class="card-title">{{ t('projects.recentBuildsTitle') }}</h2>
      </div>

      <n-alert v-if="runsError" type="error" :title="runsError" role="alert" class="card-alert">
        <button type="button" class="btn-outline" data-testid="runs-retry" @click="loadRecent">
          {{ t('plines.retry') }}
        </button>
      </n-alert>

      <div v-else-if="runsLoading" class="card-skeleton">
        <n-skeleton text :repeat="4" height="40px" />
      </div>

      <template v-else-if="runs.length > 0">
        <div class="runs-head">
          <span class="rc-name">{{ t('plines.colPipeline') }}</span>
          <span class="rc-status">{{ t('overview.colStatus') }}</span>
          <span class="rc-trigger">{{ t('overview.colTrigger') }}</span>
          <span class="rc-duration">{{ t('overview.colDuration') }}</span>
          <span class="rc-time">{{ t('overview.colTime') }}</span>
        </div>
        <button
          v-for="row in runs"
          :key="`${row.pipeline_name}-${row.number}`"
          type="button"
          class="run-row"
          :data-testid="`run-row-${row.pipeline_name}-${row.number}`"
          @click="openBuild(row)"
        >
          <span class="rc-name">
            <span class="run-name">
              {{ row.pipeline_name }}<span class="build-no">#{{ row.number }}</span>
            </span>
            <span class="run-meta">{{ row.trigger_by }}</span>
          </span>
          <span class="rc-status">
            <span class="badge" :class="statusBadgeClass(row.status)">
              {{ t(`buildStatus.${row.status}`) }}
            </span>
          </span>
          <span class="rc-trigger">{{ t(`triggerSource.${row.trigger}`) }}</span>
          <span class="rc-duration">{{ formatDuration(runDuration(row)) }}</span>
          <span class="rc-time">{{ relativeTimeText(row.finished_at ?? row.started_at) }}</span>
        </button>
      </template>

      <div v-else class="runs-empty" data-testid="runs-empty">
        <n-empty :description="t('overview.recentBuildsEmpty')" />
      </div>
    </section>

    <!-- 成员卡（项目 admin 档；403 卡内退化提示）。 -->
    <section class="sisy-card project-members-card" aria-label="project members">
      <div class="card-header">
        <h2 class="card-title">{{ t('projects.members') }}</h2>
      </div>

      <div v-if="!membersDone" class="card-skeleton">
        <n-skeleton text :repeat="2" height="36px" />
      </div>
      <n-alert v-else-if="memberError" type="error" :title="memberError" role="alert" class="card-alert">
        <button type="button" class="btn-outline" data-testid="members-retry" @click="loadMembers">
          {{ t('plines.retry') }}
        </button>
      </n-alert>
      <div v-else-if="memberForbidden" class="card-hint" data-testid="members-forbidden">
        <p class="form-hint">{{ t('projects.membersAdminOnly') }}</p>
      </div>

      <template v-else-if="members">
        <div v-for="m in members" :key="m.username" class="member-row" :data-testid="`member-row-${m.username}`">
          <span class="member-name">{{ m.username }}</span>
          <n-select
            :value="m.role"
            size="small"
            :options="ROLE_OPTIONS"
            :virtual-scroll="false"
            class="member-role-select"
            @update:value="(v: MemberRoleDto) => (m.role = v)"
          />
          <button
            type="button"
            class="btn-outline red"
            :data-testid="`member-remove-${m.username}`"
            @click="removeMember(m.username)"
          >
            {{ t('projects.memberRemove') }}
          </button>
        </div>

        <div class="member-add-row">
          <n-form-item :label="t('projects.memberUsername')" class="member-add-field">
            <n-select
              v-model:value="newMember"
              name="member-username"
              :placeholder="t('projects.memberSelectPlaceholder')"
              :options="addableUsers.map((u) => ({ label: u, value: u }))"
              :virtual-scroll="false"
              clearable
              data-testid="member-add-user"
            />
          </n-form-item>
          <n-form-item :label="t('projects.memberRole')" class="member-add-field">
            <n-select
              v-model:value="newRole"
              name="member-role"
              :options="ROLE_OPTIONS"
              :virtual-scroll="false"
              data-testid="member-add-role"
            />
          </n-form-item>
        </div>

        <div class="member-actions">
          <n-button
            type="primary"
            name="member-save"
            :disabled="savingMembers"
            :loading="savingMembers"
            data-testid="member-save"
            @click="saveMembers"
          >
            {{ savingMembers ? t('projects.saving') : t('projects.saveMembers') }}
          </n-button>
        </div>
        <p class="form-hint">{{ t('projects.membersReplaceHint') }}</p>
      </template>
    </section>

    <!-- SCM 凭据卡（项目 admin 档；username + password 皆空 = 清除）。 -->
    <section class="sisy-card project-cred-card" aria-label="scm credentials">
      <div class="card-header">
        <h2 class="card-title">{{ t('projects.scmCredTitle') }}</h2>
      </div>

      <div v-if="!membersDone" class="card-skeleton">
        <n-skeleton text :repeat="2" height="36px" />
      </div>
      <div v-else-if="memberForbidden" class="card-hint" data-testid="cred-forbidden">
        <p class="form-hint">{{ t('projects.credentialAdminOnly') }}</p>
      </div>
      <div v-else-if="!isProjectAdmin" class="card-hint">
        <!-- 成员面非 403 失败（500 等）：凭据卡不造假呈现「需 admin 档」。 -->
      </div>
      <n-alert v-else-if="credError" type="error" :title="credError" role="alert" class="card-alert" />

      <template v-else>
        <div class="cred-form">
          <n-form-item :label="t('projects.scmUsername')">
            <n-input
              v-model:value="credUsername"
              name="cred-username"
              :input-props="{ name: 'cred-username', autocomplete: 'off' }"
              :placeholder="t('projects.scmUsernamePlaceholder')"
            />
          </n-form-item>
          <n-form-item :label="t('projects.scmPassword')">
            <n-input
              v-model:value="credPassword"
              type="password"
              show-password-on="mousedown"
              name="cred-password"
              :input-props="{ name: 'cred-password', autocomplete: 'new-password' }"
              :placeholder="t('projects.scmPasswordPlaceholder')"
            />
          </n-form-item>
          <p class="form-hint">{{ t('projects.scmCredentialHint') }}</p>

          <div class="member-actions">
            <n-button
              name="cred-test-connection"
              :disabled="testingCred"
              :loading="testingCred"
              data-testid="cred-test"
              @click="testCredential"
            >
              {{ testingCred ? t('projects.credentialProbing') : t('projects.testConnectionExisting') }}
            </n-button>
            <n-button
              type="primary"
              name="cred-save"
              :disabled="savingCred"
              :loading="savingCred"
              data-testid="cred-save"
              @click="saveCredential"
            >
              {{ savingCred ? t('projects.saving') : t('projects.saveScmCredential') }}
            </n-button>
          </div>

          <!-- 测试连接徽章（探测中 → 成功/失败）。 -->
          <div v-if="testingCred || credProbeState" class="cred-badge" role="status">
            <n-skeleton v-if="testingCred" text width="160px" />
            <span v-else class="badge" :class="credProbeState === 'success' ? 'success' : 'failed'">
              {{ credProbeMsg }}
            </span>
          </div>
        </div>
      </template>
    </section>

    <!-- 编辑项目弹窗（契约先行 PATCH；项目 admin 档）。 -->
    <n-modal
      v-model:show="editOpen"
      preset="card"
      :title="t('projects.editProjectTitle')"
      style="width: 480px"
      :bordered="false"
    >
      <n-form-item :label="t('projects.scmUrl')">
        <n-input
          v-model:value="editUrl"
          :input-props="{ name: 'edit-scm-url', autocomplete: 'off' }"
          :placeholder="t('projects.scmUrlPlaceholder')"
        />
      </n-form-item>
      <n-form-item v-if="project.scm_type === 'git'" :label="t('projects.metaDefaultBranch')">
        <n-input
          v-model:value="editBranch"
          :input-props="{ name: 'edit-default-branch', autocomplete: 'off' }"
          :placeholder="t('projects.defaultBranchPlaceholder')"
        />
      </n-form-item>

      <p v-if="editError" class="pdl-error" role="alert">{{ editError }}</p>

      <div class="modal-actions">
        <n-button @click="editOpen = false">
          {{ t('common.cancel') }}
        </n-button>
        <n-button
          type="primary"
          :disabled="editSaving"
          :loading="editSaving"
          data-testid="edit-project-save"
          @click="submitEditProject"
        >
          {{ editSaving ? t('projects.saving') : t('projects.save') }}
        </n-button>
      </div>
    </n-modal>

    <!-- 新建流水线弹窗（输名 → 编辑器；保存即创建语义在编辑器侧）。 -->
    <n-modal
      v-model:show="newOpen"
      preset="card"
      :title="t('plines.newPipeline')"
      style="width: 400px"
      :bordered="false"
    >
      <n-form-item :label="t('projects.newPipelineName')">
        <n-input
          v-model:value="newPipelineName"
          :input-props="{ name: 'new-pipeline-name', autocomplete: 'off' }"
          :placeholder="t('projects.newPipelinePlaceholder')"
          @keyup.enter="createPipeline"
        />
      </n-form-item>

      <p v-if="newNameError" class="pdl-error" role="alert">{{ newNameError }}</p>

      <div class="modal-actions">
        <n-button @click="newOpen = false">
          {{ t('common.cancel') }}
        </n-button>
        <n-button type="primary" data-testid="new-pipeline-create" @click="createPipeline">
          {{ t('projects.newPipelineCreate') }}
        </n-button>
      </div>
    </n-modal>
  </div>
</template>

<style scoped>
.project-detail-page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.pdl-skeleton-row {
  width: 100%;
}

/* 页头：标题 + scm 徽章 + 动作。 */
.project-detail-header {
  align-items: center;
  flex-wrap: wrap;
}

.project-title-row {
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
}

.project-title {
  margin-bottom: 0;
  font-size: 22px;
}

.project-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

/* 项目信息卡（元信息条，与构建详情 meta 同形态）。 */
.meta-grid {
  margin: 0;
  display: flex;
  gap: 8px 32px;
  flex-wrap: wrap;
  padding: 0 20px 16px;
}

.meta-item {
  display: flex;
  flex-direction: column;
  gap: 3px;
  min-width: 96px;
}

.meta-item dt {
  color: var(--sisy-color-text-secondary);
  font-size: 11px;
}

.meta-item dd {
  margin: 0;
  font-size: 13px;
  font-weight: 500;
  color: var(--sisy-color-text);
  word-break: break-all;
}

.mono-url {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}

.card-alert {
  margin: 0 20px 16px;
}

.card-alert button {
  margin-top: 8px;
}

.card-skeleton {
  padding: 0 20px 16px;
}

.card-hint {
  padding: 0 20px 16px;
}

/* 流水线行（pipe-row 精简化：五列 + 动作）。 */
.ppl-thead {
  display: flex;
  align-items: center;
  padding: 0 20px;
  height: 40px;
  border-top: 1px solid var(--sisy-color-border);
  border-bottom: 1px solid var(--sisy-color-border);
}

.ppl-thead span {
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.ppl-row {
  display: flex;
  align-items: center;
  padding: 0 20px;
  min-height: 56px;
  border-bottom: 1px solid var(--sisy-color-border-light);
  transition: background 0.15s;
}

.ppl-row:last-child {
  border-bottom: none;
}

.ppl-row:hover {
  background: var(--sisy-color-bg);
}

.ppl-col-name {
  flex: 1;
  min-width: 0;
}

.ppl-name {
  display: flex;
  flex-direction: column;
  gap: 3px;
  border: none;
  background: none;
  padding: 0;
  cursor: pointer;
  text-align: left;
  font-family: inherit;
}

.ppl-name .n {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.ppl-name:hover .n {
  color: var(--sisy-color-primary);
}

.ppl-col-status {
  width: 90px;
  display: flex;
  flex-shrink: 0;
}

.ppl-col-rate,
.ppl-col-avg {
  width: 90px;
  font-size: 13px;
  color: var(--sisy-color-text);
  flex-shrink: 0;
}

.ppl-col-latest {
  width: 150px;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  flex-shrink: 0;
  white-space: nowrap;
}

.ppl-col-action {
  width: 160px;
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  flex-shrink: 0;
}

.ppl-empty {
  padding: 24px 0 32px;
}

.ppl-empty-btn {
  margin-top: 12px;
}

/* 最近构建行（工作台 run-row 同形态；副行为触发人）。 */
.runs-head {
  display: flex;
  align-items: center;
  padding: 0 20px;
  height: 40px;
  border-top: 1px solid var(--sisy-color-border);
  border-bottom: 1px solid var(--sisy-color-border);
}

.runs-head span {
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.run-row {
  display: flex;
  align-items: center;
  padding: 0 20px;
  min-height: 56px;
  border: none;
  border-bottom: 1px solid var(--sisy-color-border-light);
  background: none;
  font-family: inherit;
  text-align: left;
  cursor: pointer;
  transition: background 0.15s;
  width: 100%;
}

.run-row:last-child {
  border-bottom: none;
}

.run-row:hover {
  background: var(--sisy-color-bg);
}

.rc-name {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

.run-name {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
  display: flex;
  align-items: baseline;
  gap: 6px;
}

.run-name .build-no {
  font-size: 11px;
  font-weight: 400;
  color: var(--sisy-color-text-secondary);
}

.run-meta {
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.rc-status {
  width: 84px;
  display: flex;
}

.rc-trigger {
  width: 80px;
  font-size: 13px;
  color: var(--sisy-color-text);
}

.rc-duration {
  width: 90px;
  font-size: 13px;
  color: var(--sisy-color-text);
}

.rc-time {
  width: 96px;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  white-space: nowrap;
}

.runs-empty {
  padding: 24px 0 32px;
}

/* 成员卡。 */
.project-members-card {
  padding-bottom: 16px;
}

.member-row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 8px 20px;
  border-bottom: 1px solid var(--sisy-color-border-light);
  max-width: 560px;
}

.member-row:last-of-type {
  border-bottom: none;
}

.member-name {
  flex: 1;
  min-width: 0;
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.member-role-select {
  width: 140px;
  flex-shrink: 0;
}

.member-add-row {
  display: flex;
  gap: 12px;
  padding: 12px 20px 0;
  max-width: 560px;
}

.member-add-field {
  flex: 1;
}

.member-actions {
  display: flex;
  gap: 8px;
  padding: 0 20px;
  margin: 8px 0;
}

.project-members-card > .form-hint,
.cred-form > .form-hint {
  padding: 0 20px;
}

/* SCM 凭据卡。 */
.cred-form {
  max-width: 480px;
  padding: 0 20px 16px;
}

.cred-form .form-hint {
  padding: 0;
}

.cred-form .member-actions {
  padding: 0;
}

.cred-badge {
  display: flex;
  align-items: center;
  min-height: 24px;
  margin-top: 4px;
}

.pdl-error {
  margin: 0;
  color: var(--sisy-color-danger-text);
  font-size: 13px;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding-top: 8px;
}

/* G2 条令沿用（票 #105 平板档降级）：≤1024px 收起「最近运行」；≤880px 再收起
   「成功率/平均耗时」，保留 名称/状态/动作。最近构建表同源：≤900 收起时间，
   ≤780 再收起触发源（票 #104 G2 口径）。 */
@media (max-width: 1024px) {
  .ppl-col-latest {
    display: none;
  }
}

@media (max-width: 900px) {
  .runs-head .rc-time,
  .run-row .rc-time {
    display: none;
  }
}

@media (max-width: 880px) {
  .ppl-col-rate,
  .ppl-col-avg {
    display: none;
  }

  .ppl-col-action {
    width: auto;
  }
}

@media (max-width: 780px) {
  .runs-head .rc-trigger,
  .run-row .rc-trigger {
    display: none;
  }
}
</style>
