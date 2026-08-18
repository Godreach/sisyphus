<script setup lang="ts">
// 项目详情（ADR-0014/0016/0020，票 B4-T3）：pipeline 列表 + 成员角色。
//
// - pipeline 列表：后端暂无 pipeline 列表端点（`GET .../pipelines/{name}`
//   只按名取定义），本页以「已知 pipeline 名的定义探测」降级：`GET
//   .../pipelines/{pipeline}` 200 = 存在、404 = 不存在——探测逐个显式标注
//   退化（「pipeline 列表端点未交付，当前为逐个探测」），端点交付后换真列表。
// - 成员角色：`GET /projects/{name}/members` + `PUT`（整组替换语义）+ `GET
//   /users/directory` 下拉（ADR-0014：成员管理需项目 admin 档，403 就地展示）。

import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { projectsApi, usersApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { MemberAssignment, MemberResponse, MemberRoleDto, ProjectResponse } from '@/api/types'

const { t } = useI18n()
const route = useRoute()

const projectName = computed(() => String(route.params.name ?? ''))
const project = ref<ProjectResponse | null>(null)
const loadError = ref('')

/** pipeline 列表（降级探测：逐名探测定义 200/404）。 */
const probePipelines = ref(['main', 'release'])
const pipelines = ref<{ name: string; exists: boolean | null }[]>([])

/** 成员管理（项目 admin 档；403 就地展示）。 */
const members = ref<MemberResponse[] | null>(null)
const directory = ref<string[]>([])
const memberError = ref('')
const memberNote = ref('')
/** 成员编辑表单：下拉选用户名 + 角色；提交为整组替换（含现存成员）。 */
const newMember = ref('')
const newRole = ref<MemberRoleDto>('viewer')
const savingMembers = ref(false)

onMounted(async () => {
  await Promise.all([loadProject(), probePipelineList(), loadMembers()])
})

/** 项目元数据（viewer 档）。 */
async function loadProject(): Promise<void> {
  try {
    project.value = await projectsApi.get(projectName.value)
  } catch (err) {
    loadError.value = describeSubmitError(err)
  }
}

/** pipeline 列表降级探测：逐个 `GET .../pipelines/{name}`（200 存在 /
 *  404 不存在）。非 404 失败（网络层等）标 `exists: null` = 未知，整段
 *  标注退化信息——不把「探测失败」静默当成「未配置」（非事实不当事实）。
 *  端点交付后换真列表。 */
async function probePipelineList(): Promise<void> {
  const results = await Promise.all(
    probePipelines.value.map(async (name) => {
      try {
        await projectsApi.getPipeline(projectName.value, name)
        return { name, exists: true }
      } catch (err) {
        if (err instanceof ApiError && err.status === 404) {
          return { name, exists: false }
        }
        return { name, exists: null }
      }
    }),
  )
  pipelines.value = results
}

/** 加载成员 + 用户目录（成员管理区；非项目 admin 403 就地展示）。 */
async function loadMembers(): Promise<void> {
  memberError.value = ''
  memberNote.value = ''
  try {
    const [memberRows, dir] = await Promise.all([projectsApi.listMembers(projectName.value), usersApi.directory()])
    members.value = memberRows
    directory.value = dir.map((d) => d.username)
  } catch (err) {
    members.value = null
    memberError.value = describeSubmitError(err)
  }
}

/** 提交成员整组替换（PUT 语义：当前表单成员 = 完整清单，未列入者移除）。
 *  成功回读成员清单。 */
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
    memberNote.value = t('projects.membersSaved')
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
</script>

<template>
  <div class="project-detail-page">
    <h1 class="page-title">{{ project?.name ?? projectName }}</h1>

    <p v-if="loadError" class="form-error" role="alert">{{ loadError }}</p>

    <dl v-if="project" class="project-meta-dl">
      <dt>{{ t('projects.scmType') }}</dt>
      <dd>{{ project.scm_type }}</dd>
      <dt>{{ t('projects.scmUrl') }}</dt>
      <dd class="mono">{{ project.scm_url }}</dd>
      <dt v-if="project.default_branch">{{ t('projects.defaultBranch') }}</dt>
      <dd v-if="project.default_branch">{{ project.default_branch }}</dd>
    </dl>

    <!-- pipeline 列表（降级探测 + 显式标注退化）。 -->
    <section class="detail-section">
      <h2>{{ t('projects.pipelines') }}</h2>
      <p v-if="loadError" class="form-error" role="alert">{{ loadError }}</p>
      <ul class="pipeline-list">
        <li v-for="p in pipelines" :key="p.name" class="pipeline-item">
          <span class="pipeline-name">{{ p.name }}</span>
          <span v-if="p.exists === true" class="badge badge-ok">{{ t('projects.pipelineExists') }}</span>
          <span v-else-if="p.exists === false" class="badge">{{ t('projects.pipelineMissing') }}</span>
          <span v-else class="badge badge-unknown">{{ t('projects.pipelineUnknown') }}</span>
        </li>
      </ul>
      <p class="form-hint">{{ t('projects.pipelineListDegraded') }}</p>
    </section>

    <!-- 成员角色（ADR-0014：项目 admin 档整组分配 viewer/runner/admin）。 -->
    <section class="detail-section">
      <h2>{{ t('projects.members') }}</h2>
      <p v-if="memberError" class="form-error" role="alert">{{ memberError }}</p>

      <form v-if="members" class="member-form" @submit.prevent>
        <table class="member-table">
          <thead>
            <tr>
              <th>{{ t('projects.memberUsername') }}</th>
              <th>{{ t('projects.memberRole') }}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="m in members" :key="m.username">
              <td>{{ m.username }}</td>
              <td>
                <select :value="m.role" @change="m.role = ($event.target as HTMLSelectElement).value as MemberRoleDto">
                  <option value="viewer">viewer</option>
                  <option value="runner">runner</option>
                  <option value="admin">admin</option>
                </select>
              </td>
              <td>
                <button type="button" class="btn-secondary" @click="removeMember(m.username)">
                  {{ t('projects.memberRemove') }}
                </button>
              </td>
            </tr>
          </tbody>
        </table>

        <div class="member-add-row">
          <label class="field">
            <span>{{ t('projects.memberUsername') }}</span>
            <select v-model="newMember" name="member-username">
              <option value="" disabled>{{ t('projects.memberSelectPlaceholder') }}</option>
              <option v-for="u in directory" :key="u" :value="u">{{ u }}</option>
            </select>
          </label>
          <label class="field">
            <span>{{ t('projects.memberRole') }}</span>
            <select v-model="newRole" name="member-role">
              <option value="viewer">viewer</option>
              <option value="runner">runner</option>
              <option value="admin">admin</option>
            </select>
          </label>
        </div>

        <div class="member-actions">
          <button type="button" class="btn-primary" :disabled="savingMembers" @click="saveMembers">
            {{ savingMembers ? t('projects.saving') : t('projects.saveMembers') }}
          </button>
        </div>
        <p v-if="memberNote" class="form-hint" role="status">{{ memberNote }}</p>
        <p class="form-hint">{{ t('projects.membersReplaceHint') }}</p>
      </form>

      <p v-else-if="!memberError" class="form-hint">{{ t('projects.membersAdminOnly') }}</p>
    </section>
  </div>
</template>
