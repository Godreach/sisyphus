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

import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { projectsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ProjectResponse, ScmTypeDto } from '@/api/types'

const { t } = useI18n()
const router = useRouter()

const projects = ref<ProjectResponse[] | null>(null)
const listError = ref('')

/** 新建表单。 */
const showForm = ref(false)
const name = ref('')
const scmType = ref<ScmTypeDto>('git')
const scmUrl = ref('')
const defaultBranch = ref('')
const scmUsername = ref('')
const scmPassword = ref('')
const submitting = ref(false)
const submitError = ref('')

/** 测试连接态（不阻塞保存）。 */
const probing = ref(false)
const probeMsg = ref('')
const probeError = ref('')

const canSubmit = computed(
  () => name.value.trim() !== '' && scmUrl.value.trim() !== '' && !submitting.value,
)

onMounted(load)

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
 *  成功展示 head、git 空默认分支时预填；失败展示可读错误（凭据不回显）。 */
async function testConnection(): Promise<void> {
  probeError.value = ''
  probeMsg.value = ''
  if (scmUrl.value.trim() === '') {
    probeError.value = t('projects.scmUrlRequired')
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
    probeError.value = describeSubmitError(err)
  } finally {
    probing.value = false
  }
}

/** 新建项目：`POST /projects`（全局 admin 专属；403 就地展示）。成功即
 *  收表单 + 刷新列表。 */
async function createProject(): Promise<void> {
  submitError.value = ''
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
    scmUrl.value = ''
    defaultBranch.value = ''
    scmUsername.value = ''
    scmPassword.value = ''
    probeMsg.value = ''
    probeError.value = ''
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
</script>

<template>
  <div class="projects-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.projects') }}</h1>
      <button type="button" class="btn-primary" name="project-new" @click="showForm = !showForm">
        {{ t('projects.newProject') }}
      </button>
    </div>

    <p v-if="listError" class="form-error" role="alert">{{ listError }}</p>

    <!-- 新建项目表单（git/svn + 仓库 URL + git 默认分支 + 可选 SCM 凭据）。 -->
    <form v-if="showForm" class="project-form" @submit.prevent>
      <label class="field">
        <span>{{ t('projects.name') }}</span>
        <input v-model="name" name="project-name" />
      </label>
      <label class="field">
        <span>{{ t('projects.scmType') }}</span>
        <select v-model="scmType" name="project-scm-type">
          <option value="git">git</option>
          <option value="svn">svn</option>
        </select>
      </label>
      <label class="field">
        <span>{{ t('projects.scmUrl') }}</span>
        <input v-model="scmUrl" name="project-url" :placeholder="t('projects.scmUrlPlaceholder')" />
      </label>

      <label v-if="scmType === 'git'" class="field">
        <span>{{ t('projects.defaultBranch') }}</span>
        <input v-model="defaultBranch" name="project-branch" :placeholder="t('projects.defaultBranchPlaceholder')" />
      </label>

      <!-- SCM 凭据（可选，加密落库；仅私有 https 仓库需填）。 -->
      <label class="field">
        <span>{{ t('projects.scmUsername') }}</span>
        <input
          v-model="scmUsername"
          name="project-scm-username"
          autocomplete="off"
          :placeholder="t('projects.scmUsernamePlaceholder')"
        />
      </label>
      <label class="field">
        <span>{{ t('projects.scmPassword') }}</span>
        <input
          v-model="scmPassword"
          name="project-scm-password"
          type="password"
          autocomplete="new-password"
          :placeholder="t('projects.scmPasswordPlaceholder')"
        />
      </label>
      <p class="form-hint">{{ t('projects.scmCredHint') }}</p>

      <div class="project-form-actions">
        <!-- 测试连接不阻塞保存（ADR-0016）：验证 URL+凭据、预填默认分支。 -->
        <button
          type="button"
          class="btn-secondary"
          name="project-test-connection"
          :disabled="probing"
          @click="testConnection"
        >
          {{ probing ? t('projects.probing') : t('projects.testConnection') }}
        </button>
        <button
          type="button"
          class="btn-primary"
          name="project-save"
          :disabled="!canSubmit"
          @click="createProject"
        >
          {{ submitting ? t('projects.saving') : t('projects.save') }}
        </button>
      </div>
      <p v-if="scmType === 'git'" class="form-hint">{{ t('projects.branchPrefillHint') }}</p>
      <p class="form-hint">{{ t('projects.testConnectionHint') }}</p>

      <p v-if="probeMsg" class="form-hint" role="status">{{ probeMsg }}</p>
      <p v-if="probeError" class="form-error" role="alert">{{ probeError }}</p>
      <p v-if="submitError" class="form-error" role="alert">{{ submitError }}</p>
    </form>

    <!-- 项目列表（可见性过滤）。 -->
    <ul v-if="projects" class="project-list">
      <li v-for="p in projects" :key="p.name" class="project-item">
        <button type="button" class="project-link" @click="openProject(p)">
          <span class="project-name">{{ p.name }}</span>
          <span class="project-meta">{{ p.scm_type }} · {{ p.scm_url }}</span>
        </button>
      </li>
    </ul>
    <p v-else-if="!listError" class="form-hint">{{ t('projects.empty') }}</p>
  </div>
</template>
