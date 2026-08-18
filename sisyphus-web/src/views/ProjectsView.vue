<script setup lang="ts">
// 项目列表 + 新建（ADR-0016/0020，票 B4-T3）。
//
// - 列表：`GET /projects`（可见性过滤），点击进项目详情。
// - 新建：`POST /projects`（git/svn + 仓库 URL + git 可选默认分支），建
//   项目为全局 admin 专属（403 按统一错误形态就地展示）。
// - 测试连接（ADR-0016「不阻塞保存」）：**测试连接端点尚未交付** —— 按钮按
//   「不阻塞保存」的禁用/提示态处理（Spec B4 决策 2 + 票 B4-T3）：置灰、
//   旁注「测试连接端点未交付」；保存不依赖该动作。ls-remote 预填默认分支
//   同理：端点未交付，分支字段为手动输入 + 提示（预填端点交付后在保存前
//   自动解析）。

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
const submitting = ref(false)
const submitError = ref('')

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
    })
    showForm.value = false
    name.value = ''
    scmUrl.value = ''
    defaultBranch.value = ''
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

    <!-- 新建项目表单（git/svn + 仓库 URL + git 默认分支）。 -->
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

      <div class="project-form-actions">
        <!-- 测试连接不阻塞保存（ADR-0016）。端点未交付：禁用 + 提示态。 -->
        <button type="button" class="btn-secondary" disabled :title="t('projects.testConnectionUnavailable')">
          {{ t('projects.testConnection') }}
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
