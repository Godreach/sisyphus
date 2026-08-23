<script setup lang="ts">
// 项目详情（ADR-0014/0016/0020，票 B4-T3）：pipeline 列表 + 成员角色 + SCM 凭据。
//
// - pipeline 列表：后端暂无 pipeline 列表端点（`GET .../pipelines/{name}`
//   只按名取定义），本页以「已知 pipeline 名的定义探测」降级：`GET
//   .../pipelines/{pipeline}` 200 = 存在、404 = 不存在——探测逐个显式标注
//   退化（「pipeline 列表端点未交付，当前为逐个探测」），端点交付后换真列表。
// - 成员角色：`GET /projects/{name}/members` + `PUT`（整组替换语义）+ `GET
//   /users/directory` 下拉（ADR-0014：成员管理需项目 admin 档，403 就地展示）。
// - SCM 凭据：`PUT /projects/{name}/scm-credential`（整组替换；username +
//   password 皆空 = 清除）+ `POST /projects/{name}/test-connection`（存储凭据
//   探测 head）。项目 admin 档，403 就地展示。
// #92: 使用 Naive UI 组件重写——概览 / Pipeline / 成员 / SCM 凭据 用 NTabs
// 组织；pipeline 状态用 NTag、成员表用 NDataTable、表单用 NForm；视觉与
// #84/#86 主题一致。

import { computed, h, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NTabs,
  NTabPane,
  NCard,
  NTag,
  NButton,
  NInput,
  NSelect,
  NForm,
  NFormItem,
  NDataTable,
  NAlert,
  NSpin,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'

import { projectsApi, usersApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { MemberAssignment, MemberResponse, MemberRoleDto, ProjectResponse } from '@/api/types'

const { t } = useI18n()
const route = useRoute()
const message = useMessage()

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
/** 当前用户是否有项目 admin 档（成员清单可读 = 是；403 = 否）。 */
const isProjectAdmin = ref(false)
/** 成员编辑表单：下拉选用户名 + 角色；提交为整组替换（含现存成员）。 */
const newMember = ref('')
const newRole = ref<MemberRoleDto>('viewer')
const savingMembers = ref(false)

/** SCM 凭据（项目 admin 档；username + password 皆空 = 清除）。 */
const credUsername = ref('')
const credPassword = ref('')
const savingCred = ref(false)
const credError = ref('')
const testingCred = ref(false)
const credProbeState = ref<'success' | 'error' | null>(null)
const credProbeMsg = ref('')

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

/** 加载成员 + 用户目录（成员管理区；非项目 admin 403 就地展示）。
 *  成员清单可读（200）即当前用户具备项目 admin 档，SCM 凭据标签页据此门控。 */
async function loadMembers(): Promise<void> {
  memberError.value = ''
  memberNote.value = ''
  isProjectAdmin.value = false
  try {
    const [memberRows, dir] = await Promise.all([projectsApi.listMembers(projectName.value), usersApi.directory()])
    members.value = memberRows
    directory.value = dir.map((d) => d.username)
    isProjectAdmin.value = true
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

/** 成员 NDataTable 列（角色选择器内联；移除动作）。 */
const memberColumns = computed<DataTableColumns<MemberResponse>>(() => [
  {
    title: t('projects.memberUsername'),
    key: 'username',
  },
  {
    title: t('projects.memberRole'),
    key: 'role',
    render: (row) =>
      h(
        NSelect,
        {
          size: 'small',
          value: row.role,
          options: [
            { label: 'viewer', value: 'viewer' },
            { label: 'runner', value: 'runner' },
            { label: 'admin', value: 'admin' },
          ],
          virtualScroll: false,
          onUpdateValue: (v: MemberRoleDto) => {
            row.role = v
          },
        },
      ),
  },
  {
    title: '',
    key: 'actions',
    width: 90,
    render: (row) =>
      h(
        NButton,
        { size: 'small', quaternary: true, type: 'error', onClick: () => removeMember(row.username) },
        { default: () => t('projects.memberRemove') },
      ),
  },
])

const memberRowKey = (row: MemberResponse): string => row.username

/** SCM 凭据：保存（PUT 整组替换；皆空 = 清除）。成功 toast + 清空表单。 */
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

/** SCM 凭据：测试连接（存储凭据探测 head；NSpin → NTag 徽章）。 */
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
</script>

<template>
  <div class="project-detail-page">
    <h1 class="page-title">{{ project?.name ?? projectName }}</h1>

    <n-alert v-if="loadError" type="error" :title="loadError" role="alert" class="detail-alert" />

    <n-tabs v-if="project" type="line" animated class="project-tabs">
      <!-- 概览：项目元数据 + SCM 凭据入口。 -->
      <n-tab-pane name="overview" :tab="t('projects.tabOverview')">
        <dl class="project-meta-dl">
          <dt>{{ t('projects.scmType') }}</dt>
          <dd>{{ project.scm_type }}</dd>
          <dt>{{ t('projects.scmUrl') }}</dt>
          <dd class="mono">{{ project.scm_url }}</dd>
          <dt v-if="project.default_branch">{{ t('projects.defaultBranch') }}</dt>
          <dd v-if="project.default_branch">{{ project.default_branch }}</dd>
        </dl>
      </n-tab-pane>

      <!-- Pipeline 列表（降级探测 + 显式标注退化）。 -->
      <n-tab-pane name="pipelines" :tab="t('projects.tabPipelines')">
        <div class="pipeline-list">
          <n-card v-for="p in pipelines" :key="p.name" size="small" class="pipeline-item">
            <template #header>
              <span class="pipeline-name">{{ p.name }}</span>
            </template>
            <template #header-extra>
              <n-tag
                v-if="p.exists === true"
                type="success"
                size="small"
                :bordered="false"
              >
                {{ t('projects.pipelineExists') }}
              </n-tag>
              <n-tag v-else-if="p.exists === false" size="small" :bordered="false">
                {{ t('projects.pipelineMissing') }}
              </n-tag>
              <n-tag v-else type="warning" size="small" :bordered="false">
                {{ t('projects.pipelineUnknown') }}
              </n-tag>
            </template>
          </n-card>
        </div>
        <p class="form-hint">{{ t('projects.pipelineListDegraded') }}</p>
      </n-tab-pane>

      <!-- 成员角色（ADR-0014：项目 admin 档整组分配 viewer/runner/admin）。 -->
      <n-tab-pane name="members" :tab="t('projects.tabMembers')">
        <n-alert v-if="memberError" type="error" :title="memberError" role="alert" class="detail-alert" />

        <template v-if="members">
          <n-data-table
            :columns="memberColumns"
            :data="members"
            :row-key="memberRowKey"
            :bordered="false"
            :single-line="true"
            size="small"
            class="member-table"
          />

          <div class="member-add-row">
            <n-form-item :label="t('projects.memberUsername')" class="member-add-field">
              <n-select
                v-model:value="newMember"
                name="member-username"
                :placeholder="t('projects.memberSelectPlaceholder')"
                :options="directory.map((u) => ({ label: u, value: u }))"
                :virtual-scroll="false"
              />
            </n-form-item>
            <n-form-item :label="t('projects.memberRole')" class="member-add-field">
              <n-select
                v-model:value="newRole"
                name="member-role"
                :options="[
                  { label: 'viewer', value: 'viewer' },
                  { label: 'runner', value: 'runner' },
                  { label: 'admin', value: 'admin' },
                ]"
                :virtual-scroll="false"
              />
            </n-form-item>
          </div>

          <div class="member-actions">
            <n-button
              type="primary"
              name="member-save"
              :disabled="savingMembers"
              :loading="savingMembers"
              @click="saveMembers"
            >
              {{ savingMembers ? t('projects.saving') : t('projects.saveMembers') }}
            </n-button>
          </div>
          <p v-if="memberNote" class="form-hint" role="status">{{ memberNote }}</p>
          <p class="form-hint">{{ t('projects.membersReplaceHint') }}</p>
        </template>

        <p v-else-if="!memberError" class="form-hint">{{ t('projects.membersAdminOnly') }}</p>
      </n-tab-pane>

      <!-- SCM 凭据（项目 admin 档；username + password 皆空 = 清除）。 -->
      <n-tab-pane name="scm-credential" :tab="t('projects.tabScmCredential')">
        <p v-if="!isProjectAdmin && !credError" class="form-hint">{{ t('projects.credentialAdminOnly') }}</p>
        <n-alert v-else-if="credError" type="error" :title="credError" role="alert" class="detail-alert" />

        <template v-else>
          <n-form label-placement="top" class="cred-form">
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
                @click="testCredential"
              >
                {{ testingCred ? t('projects.credentialProbing') : t('projects.testConnectionExisting') }}
              </n-button>
              <n-button
                type="primary"
              name="cred-save"
              :disabled="savingCred"
              :loading="savingCred"
              @click="saveCredential"
            >
              {{ savingCred ? t('projects.saving') : t('projects.saveScmCredential') }}
            </n-button>
          </div>

          <!-- 测试连接徽章（NSpin 探测中 → NTag 成功/失败）。 -->
          <div v-if="testingCred || credProbeState" class="cred-badge" role="status">
            <n-spin v-if="testingCred" size="small" />
            <n-tag
              v-else-if="credProbeState"
              :type="credProbeState === 'success' ? 'success' : 'error'"
              size="small"
              :bordered="false"
              round
            >
              {{ credProbeMsg }}
            </n-tag>
          </div>
          </n-form>
        </template>
      </n-tab-pane>
    </n-tabs>
  </div>
</template>

<style scoped>
.project-meta-dl {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 4px 12px;
  margin: 8px 0 0;
}

.project-meta-dl dt {
  color: var(--n-text-color-3, #999);
}

.project-meta-dl dd {
  margin: 0;
}

.detail-alert {
  margin: 12px 0;
}

.pipeline-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 12px 0;
}

/* #98: 自 main.css 收编，仅迁实际生效的布局声明——background / border /
 * border-radius 在全局 (0,1,0) 时即输给 .n-card 主题 token（死声明），
 * 不随迁，避免 scoped (0,2,0) 反把它们复活成回归。 */
.pipeline-item {
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
}

.pipeline-name {
  font-weight: 600;
  font-size: 14px;
}

/* #98: 自 main.css 收编。width:100% 由 .n-data-table 根规则提供；
 * border-collapse 对 div 根是无效声明；margin 取原 scoped 覆盖后的生效值。 */
.member-table {
  max-width: 560px;
  margin: 12px 0;
}

/* #98: 原 `.member-table th/td` 四条声明中仅 font-size 实际生效——
 * padding / text-align / border-bottom 输给 .n-data-table 内部规则
 * `.n-data-table .n-data-table-th/td`（(0,2,0) > 全局 (0,1,1)），由主题
 * token 提供。内部单元格需 :deep 穿透组件边界。 */
.member-table :deep(th),
.member-table :deep(td) {
  font-size: 13px;
}

.member-add-row {
  display: flex;
  gap: 12px;
  margin: 12px 0;
  max-width: 560px;
}

.member-add-field {
  flex: 1;
}

.member-actions {
  display: flex;
  gap: 8px;
  margin: 8px 0;
}

.cred-form {
  max-width: 480px;
}

.cred-badge {
  display: flex;
  align-items: center;
  min-height: 24px;
  margin-top: 8px;
}
</style>
