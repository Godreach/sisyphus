<script setup lang="ts">
// 项目列表 + 新建（ADR-0016/0020，票 B4-T3 / B5-T3）。
//
// - 列表：`GET /projects`（可见性过滤），点击进项目详情。
// - 新建：`POST /projects`（git/svn + 仓库 URL + git 可选默认分支 + 可选 SCM
//   凭据），建项目为全局 admin 专属（403 就地展示）。
// - 测试连接（B5-T3，ADR-0016「不阻塞保存」）：点「测试连接」调
//   `POST /projects/scm-probe`（验证 URL+凭据、返回当前 head）+ git 再调
//   `POST /projects/scm-branches`（ls-remote --heads + 默认分支）以预填默认
//   分支。成功展示 head、git 空默认分支时预填；失败展示可读错误（凭据不
//   回显）。保存不依赖该动作。
// #92: 使用 Naive UI 组件重写——项目列表改卡片布局（NCard，名称/SCM 类型/
// 默认分支）、创建表单用 NForm/NFormItem 带校验、测试连接显示 NSpin → NTag
// 成功/失败徽章（不离开表单）、空列表 NEmpty + 创建引导按钮、成功 toast
// 通知；视觉与 #84/#86 主题一致。

import { computed, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NCard,
  NForm,
  NFormItem,
  NInput,
  NSelect,
  NButton,
  NTag,
  NSpin,
  NEmpty,
  NAlert,
  NIcon,
  useMessage,
  type FormInst,
  type FormRules,
} from 'naive-ui'
import { GitBranch, CreateOutline } from '@vicons/ionicons5'

import { projectsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ProjectResponse, ScmTypeDto } from '@/api/types'

const { t } = useI18n()
const router = useRouter()
const route = useRoute()
const message = useMessage()

const projects = ref<ProjectResponse[] | null>(null)
const listError = ref('')
const searchQuery = computed(() => (typeof route.query.q === 'string' ? route.query.q.trim().toLowerCase() : ''))
const filteredProjects = computed(() => {
  if (projects.value === null || searchQuery.value === '') return projects.value
  return projects.value.filter((project) =>
    [project.name, project.scm_url, project.scm_type].some((value) => value.toLowerCase().includes(searchQuery.value)),
  )
})

/** 新建表单。 */
const showForm = ref(false)
const formRef = ref<FormInst | null>(null)
const name = ref('')
const scmType = ref<ScmTypeDto>('git')
const scmUrl = ref('')
const defaultBranch = ref('')
const scmUsername = ref('')
const scmPassword = ref('')
const submitting = ref(false)
const submitError = ref('')

/** 测试连接态（不阻塞保存）：NSpin → NTag 成功/失败徽章。 */
const probing = ref(false)
const probeState = ref<'success' | 'error' | null>(null)
const probeMsg = ref('')

const formModel = computed(() => ({
  name: name.value,
  scmType: scmType.value,
  scmUrl: scmUrl.value,
  defaultBranch: defaultBranch.value,
  scmUsername: scmUsername.value,
  scmPassword: scmPassword.value,
}))

const rules = computed<FormRules>(() => ({
  name: { required: true, message: t('projects.nameRequired'), trigger: 'blur' },
  scmUrl: {
    trigger: 'blur',
    validator: (_rule, value: string) => {
      if (scmType.value === 'none') return true
      if (value == null || value.trim() === '') {
        return new Error(t('projects.scmUrlRequired'))
      }
      if (!/^https?:\/\/.+/.test(value.trim())) {
        return new Error(t('projects.scmUrlInvalid'))
      }
      return true
    },
  },
}))

function consumeCreateQuery(): void {
  // 项目页的 `?create=1` 深链 → 打开新建项目弹框并清参。
  if (route.query.create === '1') {
    openForm()
    void router.replace({ query: { ...route.query, create: undefined } })
  }
}

watch(
  () => route.query.create,
  (create) => {
    if (create === '1') consumeCreateQuery()
  },
)

onMounted(() => {
  consumeCreateQuery()
  void load()
})

/** 加载项目列表（按可见性过滤）。 */
async function load(): Promise<void> {
  listError.value = ''
  try {
    projects.value = await projectsApi.list()
  } catch (err) {
    projects.value = null
    listError.value = describeSubmitError(err)
  }
}

/** 测试连接：scm-probe（head）+ git 的 scm-branches（默认分支预填）。
 *  成功展示 head、git 空默认分支时预填；失败展示可读错误（凭据不回显）。
 *  空 URL 就地提示，不发网络请求（ADR-0016「测试连接不阻塞保存」）。 */
async function testConnection(): Promise<void> {
  probeState.value = null
  probeMsg.value = ''
  if (scmType.value === 'none') return
  if (scmUrl.value.trim() === '') {
    probeState.value = 'error'
    probeMsg.value = t('projects.scmUrlRequired')
    return
  }
  probing.value = true
  try {
    const probe = await projectsApi.scmProbe({
      scm_type: scmType.value,
      scm_url: scmUrl.value.trim(),
      username: scmUsername.value.trim() || null,
      password: scmPassword.value || null,
    })
    const head = probe.head
    probeState.value = 'success'
    probeMsg.value =
      head === null
        ? t('projects.testConnectionEmpty')
        : t('projects.testConnectionOk', { head })

    // git：分支枚举预填默认分支（仅当默认分支为空时）。
    if (scmType.value === 'git') {
      const branches = await projectsApi.scmBranches({
        scm_url: scmUrl.value.trim(),
        username: scmUsername.value.trim() || null,
        password: scmPassword.value || null,
      })
      if (defaultBranch.value.trim() === '' && branches.default_branch) {
        defaultBranch.value = branches.default_branch
        probeMsg.value += ' ' + t('projects.testConnectionPrefilled', { branch: branches.default_branch })
      }
    }
  } catch (err) {
    probeState.value = 'error'
    probeMsg.value = describeSubmitError(err)
  } finally {
    probing.value = false
  }
}

/** 新建项目：`POST /projects`（全局 admin 专属；403 就地展示）。成功即
 *  收表单 + 刷新列表 + toast 通知。 */
async function createProject(): Promise<void> {
  submitError.value = ''
  try {
    await formRef.value?.validate()
  } catch {
    // 校验失败，不提交
    return
  }
  submitting.value = true
  try {
    await projectsApi.create({
      name: name.value.trim(),
      scm_type: scmType.value,
      scm_url: scmUrl.value.trim(),
      default_branch: scmType.value === 'git' ? defaultBranch.value.trim() || null : null,
      scm_username: scmUsername.value.trim() || null,
      scm_password: scmPassword.value || null,
    })
    showForm.value = false
    name.value = ''
    scmType.value = 'git'
    scmUrl.value = ''
    defaultBranch.value = ''
    scmUsername.value = ''
    scmPassword.value = ''
    probeState.value = null
    probeMsg.value = ''
    formRef.value?.restoreValidation()
    message.success(t('projects.created'))
    await load()
  } catch (err) {
    submitError.value = describeSubmitError(err)
  } finally {
    submitting.value = false
  }
}

function openProject(p: ProjectResponse): void {
  void router.push({ name: 'project-detail', params: { name: p.name } })
}

function toggleForm(): void {
  if (showForm.value) {
    showForm.value = false
    probeState.value = null
    probeMsg.value = ''
    submitError.value = ''
    formRef.value?.restoreValidation()
  } else {
    openForm()
  }
}

function openForm(): void {
  submitError.value = ''
  showForm.value = true
}

function onScmTypeChange(v: ScmTypeDto): void {
  scmType.value = v
  if (v === 'none') {
    scmUrl.value = ''
    defaultBranch.value = ''
    scmUsername.value = ''
    scmPassword.value = ''
  }
  probeState.value = null
  probeMsg.value = ''
}
</script>

<template>
  <div class="projects-page">
    <n-alert v-if="listError" type="error" :title="listError" role="alert" class="projects-alert" />

    <!-- 新建项目表单只在主动点击 CTA 后通过弹框出现。 -->
    <div v-if="showForm" class="project-modal-mask" role="presentation">
      <n-card
        class="project-modal"
        :title="t('projects.newProject')"
        role="dialog"
        aria-modal="true"
        :bordered="false"
      >
        <template #header-extra>
          <n-button quaternary circle aria-label="close" @click="toggleForm">×</n-button>
        </template>
      <n-form
        ref="formRef"
        :model="formModel"
        :rules="rules"
        label-placement="top"
        class="project-form"
        @submit.prevent="createProject"
      >
      <n-form-item path="name" :label="t('projects.name')">
        <n-input
          v-model:value="name"
          :input-props="{ name: 'project-name' }"
          :placeholder="t('projects.name')"
        />
      </n-form-item>

      <n-form-item path="scmType" :label="t('projects.scmType')">
        <n-select
          v-model:value="scmType"
          name="project-scm-type"
          :options="[
            { label: 'git', value: 'git' },
            { label: 'svn', value: 'svn' },
            { label: t('projects.scmNone'), value: 'none' },
          ]"
          :virtual-scroll="false"
          @update:value="onScmTypeChange"
        />
      </n-form-item>

      <n-form-item v-if="scmType !== 'none'" path="scmUrl" :label="t('projects.scmUrl')">
        <n-input
          v-model:value="scmUrl"
          :input-props="{ name: 'project-url' }"
          :placeholder="t('projects.scmUrlPlaceholder')"
        />
      </n-form-item>

      <n-form-item v-if="scmType === 'git'" path="defaultBranch" :label="t('projects.defaultBranch')">
        <n-input
          v-model:value="defaultBranch"
          :input-props="{ name: 'project-branch' }"
          :placeholder="t('projects.defaultBranchPlaceholder')"
        />
      </n-form-item>

      <!-- SCM 凭据（可选，加密落库；仅私有 https 仓库需填）。 -->
      <template v-if="scmType !== 'none'">
      <n-form-item path="scmUsername" :label="t('projects.scmUsername')">
        <n-input
          v-model:value="scmUsername"
          :input-props="{ name: 'project-scm-username', autocomplete: 'off' }"
          :placeholder="t('projects.scmUsernamePlaceholder')"
        />
      </n-form-item>
      <n-form-item path="scmPassword" :label="t('projects.scmPassword')">
        <n-input
          v-model:value="scmPassword"
          type="password"
          show-password-on="mousedown"
          :input-props="{ name: 'project-scm-password', autocomplete: 'new-password' }"
          :placeholder="t('projects.scmPasswordPlaceholder')"
        />
      </n-form-item>
      <p class="form-hint">{{ t('projects.scmCredHint') }}</p>
      </template>

      <div class="project-form-actions">
        <!-- 测试连接不阻塞保存（ADR-0016）：验证 URL+凭据、预填默认分支。
             徽章 NSpin → NTag 成功/失败就地展示，无需离开表单。 -->
        <n-button
          v-if="scmType !== 'none'"
          name="project-test-connection"
          :disabled="probing"
          :loading="probing"
          @click="testConnection"
        >
          {{ probing ? t('projects.probing') : t('projects.testConnection') }}
        </n-button>
        <n-button
          type="primary"
          name="project-save"
          :disabled="submitting"
          :loading="submitting"
          @click="createProject"
        >
          {{ submitting ? t('projects.saving') : t('projects.save') }}
        </n-button>
      </div>

      <!-- 测试连接徽章（NSpin 探测中 → NTag 成功/失败）。 -->
      <div v-if="probing || probeState" class="probe-badge" role="status">
        <n-spin v-if="probing" size="small" />
        <n-tag
          v-else-if="probeState"
          :type="probeState === 'success' ? 'success' : 'error'"
          size="small"
          :bordered="false"
          round
        >
          {{ probeMsg }}
        </n-tag>
      </div>

      <p v-if="scmType === 'git'" class="form-hint">{{ t('projects.branchPrefillHint') }}</p>
      <p v-if="scmType !== 'none'" class="form-hint">{{ t('projects.testConnectionHint') }}</p>

      <n-alert v-if="submitError" type="error" :title="submitError" role="alert" class="projects-alert" />
      </n-form>
      </n-card>
    </div>

    <!-- 项目列表（卡片布局；可见性过滤）。 -->
    <template v-if="filteredProjects && filteredProjects.length > 0">
      <div class="project-card-grid" aria-label="projects">
        <n-card
          v-for="p in filteredProjects"
          :key="p.name"
          class="project-card"
          size="small"
          :bordered="true"
          :hoverable="true"
          role="button"
          :tabindex="0"
          @click="openProject(p)"
          @keyup.enter="openProject(p)"
        >
          <template #header>
            <span class="project-card-name">{{ p.name }}</span>
          </template>
          <template #header-extra>
            <n-tag size="small" :bordered="false" round>{{ p.scm_type }}</n-tag>
          </template>
          <p v-if="p.scm_url" class="project-card-meta mono">{{ p.scm_url }}</p>
          <p v-else class="project-card-meta">{{ t('projects.scmNone') }}</p>
          <div v-if="p.default_branch" class="project-card-branch">
            <n-icon :component="GitBranch" class="project-card-branch-icon" />
            <span class="project-card-branch-text">{{ p.default_branch }}</span>
          </div>
        </n-card>
      </div>
    </template>

    <!-- 空项目列表：NEmpty 空状态 + 创建引导按钮。 -->
    <div v-else-if="filteredProjects && !listError" class="project-empty">
      <n-empty :description="searchQuery ? t('projects.searchEmpty') : t('projects.empty')">
        <template #extra>
          <n-button v-if="searchQuery === ''" type="primary" class="project-empty-action" @click="toggleForm">
            <template #icon>
              <n-icon :component="CreateOutline" />
            </template>
            {{ t('projects.newProject') }}
          </n-button>
        </template>
      </n-empty>
    </div>
  </div>
</template>

<style scoped>
/* #98: 自 main.css 原样收编（.n-form 非 inline 时根部无组件样式，无层叠
 * 冲突）。外围卡片样式 + 操作行布局，其余视觉由 NForm 系列提供。 */
.project-form {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 4px 0 0;
}

.project-modal-mask {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: grid;
  place-items: center;
  padding: 16px;
  background: rgba(0, 0, 0, 0.62);
}

.project-modal {
  width: min(560px, calc(100vw - 32px));
  max-height: calc(100vh - 32px);
  overflow-y: auto;
}

.project-form-actions {
  display: flex;
  gap: 8px;
  margin-top: 4px;
}

.project-card-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
  gap: 12px;
  margin: 16px 0;
}

.project-card {
  cursor: pointer;
}

.project-card-name {
  font-weight: 600;
  font-size: 15px;
}

.project-card-meta {
  color: var(--n-text-color-3, #999);
  font-size: 12px;
  word-break: break-all;
  margin: 0 0 8px;
}

.project-card-branch {
  display: flex;
  align-items: center;
  gap: 6px;
}

.project-card-branch-icon {
  color: var(--n-text-color-3, #999);
}

.project-card-branch-text {
  font-size: 12px;
  color: var(--n-text-color-2, #666);
}

.projects-alert {
  margin: 12px 0;
}

.probe-badge {
  display: flex;
  align-items: center;
  min-height: 24px;
}
</style>
