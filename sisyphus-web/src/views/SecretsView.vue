<script setup lang="ts">
// 机密管理页（ADR-0015，票 B4-T6）：项目机密「只记名不记值」纪律在 UI 成立。
//
// 管理区全局 admin 面（侧栏 is_admin 门控 + 路由守卫兜底）。机密端点为项目
// admin 档——全局 admin 隐含全部项目的项目 admin（ADR-0014），故本页以项目
// 下拉选择任一项目后管理其机密。
//
// - 列名：`GET /projects/{name}/secrets` → 仅名清单（值形态任何端点不回显）。
// - 写/覆写：`PUT /projects/{name}/secrets/{secret}` { value }（同名即覆写，
//   成功 204 无值形态）；机密名取自路径段，env 键字符集（字母数字 + `_`），
//   非法名 422。
// - 删：`DELETE /projects/{name}/secrets/{secret}`（名消失即可观察语义）。
// - 语义提示：值只写不读、永不可读回；`${}` 插值不解析机密值（防进命令串/
//   日志回显）；任务 env 键与机密名冲突在 pipeline 保存时校验。

import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import { projectsApi, secretsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ProjectResponse } from '@/api/types'

const { t } = useI18n()

const projects = ref<ProjectResponse[] | null>(null)
const projectError = ref('')
const selectedProject = ref('')

const secrets = ref<string[] | null>(null)
const secretsError = ref('')
const loadingSecrets = ref(false)

/** 写/覆写表单。 */
const newName = ref('')
const newValue = ref('')
const saving = ref(false)
const saveError = ref('')

/** 删除 busy（按名标记，禁对应按钮）。 */
const deletingName = ref<string | null>(null)

/** 最近一次写入/删除的成功提示（覆盖 vs 新建由列表长度不可见——值不回显，
 *  故以操作回执确认动作完成，不区分新建/覆写）。 */
const note = ref('')

onMounted(loadProjects)

const canSave = computed(
  () => selectedProject.value !== '' && newName.value.trim() !== '' && newValue.value !== '' && !saving.value,
)

// 403 退化态说明：本页经侧栏 `is_admin` 门控 + 路由守卫 `meta.admin` 兜底，
// 仅全局 admin 可达；而全局 admin 隐含全部项目的项目 admin 权限（ADR-0014），
// 机密端点为项目 admin 档——故对可达会话不会 403。三姊妹管理页（审计 /
// Agent 升级 / 用户）端点为全局 admin 专属，is_admin 被撤销会 403 → 各自
// 带 `adminOnly` 退化态；本页 403 不可达，故不加该分支（避免不可达死代码），
// 错误统一经下方 catch 落 `describeSubmitError` 就地展示。

/** 项目下拉：全局 admin 全量；选首个为默认。无项目 → 空态。 */
async function loadProjects(): Promise<void> {
  projectError.value = ''
  try {
    const list = await projectsApi.list()
    projects.value = list
    if (selectedProject.value === '' && list.length > 0) {
      selectedProject.value = list[0]!.name
    }
  } catch (err) {
    projects.value = null
    projectError.value = describeSubmitError(err)
  }
}

/** 加载所选项目的机密名清单（值形态任何端点不回显）。 */
async function loadSecrets(): Promise<void> {
  if (selectedProject.value === '') {
    secrets.value = null
    return
  }
  loadingSecrets.value = true
  secretsError.value = ''
  try {
    const names = await secretsApi.list(selectedProject.value)
    secrets.value = names.map((n) => n.name)
  } catch (err) {
    secrets.value = null
    secretsError.value = describeSubmitError(err)
  } finally {
    loadingSecrets.value = false
  }
}

/** 写/覆写机密：`PUT .../secrets/{secret}` { value }，成功 204 + 刷新列表。 */
async function saveSecret(): Promise<void> {
  saveError.value = ''
  note.value = ''
  saving.value = true
  try {
    await secretsApi.put(selectedProject.value, newName.value.trim(), {
      value: newValue.value,
    })
    newName.value = ''
    newValue.value = ''
    note.value = t('secrets.saved')
    await loadSecrets()
  } catch (err) {
    saveError.value = describeSubmitError(err)
  } finally {
    saving.value = false
  }
}

/** 删除机密：`DELETE .../secrets/{secret}`，成功 204 + 刷新列表。 */
async function deleteSecret(name: string): Promise<void> {
  deletingName.value = name
  note.value = ''
  try {
    await secretsApi.delete(selectedProject.value, name)
    note.value = t('secrets.deleted')
    await loadSecrets()
  } catch (err) {
    secretsError.value = describeSubmitError(err)
  } finally {
    deletingName.value = null
  }
}

/** 切换项目：重置表单。机密清单的重新加载由 `watch(selectedProject)` 驱动
 *  （v-model 改值即触发），此处不重复发请求。 */
function changeProject(): void {
  newName.value = ''
  newValue.value = ''
  saveError.value = ''
  note.value = ''
}

watch(selectedProject, () => {
  // 初始 loadProjects 设置 selectedProject 会触发本 watch 完成首次加载；
  // 手动切换同样经此路径（changeProject 仅重置表单，加载由 watch 驱动）。
  void loadSecrets()
})
</script>

<template>
  <div class="admin-page secrets-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminSecrets') }}</h1>
    </div>

    <p v-if="projectError" class="form-error" role="alert">{{ projectError }}</p>

    <!-- 无项目空态：引导先建项目（机密挂在项目下）。 -->
    <p v-else-if="projects && projects.length === 0" class="form-hint">{{ t('secrets.noProjects') }}</p>

    <template v-else-if="projects">
      <div class="secret-toolbar">
        <label class="field secret-project-field">
          <span>{{ t('secrets.project') }}</span>
          <select v-model="selectedProject" name="secret-project" @change="changeProject">
            <option v-for="p in projects" :key="p.name" :value="p.name">{{ p.name }}</option>
          </select>
        </label>
      </div>

      <!-- 写/覆写表单（名 + 值；值永不可读回，故无「当前值」回填）。 -->
      <form class="secret-form" @submit.prevent>
        <h2 class="secret-form-title">{{ t('secrets.writeTitle') }}</h2>
        <label class="field">
          <span>{{ t('secrets.name') }}</span>
          <input
            v-model="newName"
            name="secret-name"
            :placeholder="t('secrets.namePlaceholder')"
          />
        </label>
        <p class="form-hint">{{ t('secrets.nameHint') }}</p>
        <label class="field">
          <span>{{ t('secrets.value') }}</span>
          <textarea
            v-model="newValue"
            name="secret-value"
            rows="3"
            :placeholder="t('secrets.valuePlaceholder')"
          />
        </label>
        <p class="form-hint">{{ t('secrets.valueHint') }}</p>
        <div class="secret-actions">
          <button
            type="button"
            class="btn-primary"
            name="secret-save"
            :disabled="!canSave"
            @click="saveSecret"
          >
            {{ saving ? t('secrets.saving') : t('secrets.save') }}
          </button>
        </div>
        <p v-if="saveError" class="form-error" role="alert">{{ saveError }}</p>
      </form>

      <!-- 语义提示：值只写不读 + ${} 不解析 + env 键冲突（${} 经具名参数传入，
           避免 vue-i18n 把字面量 ${} 当空占位符编译）。 -->
      <p class="form-hint secret-discipline">{{ t('secrets.discipline', { interp: '${}' }) }}</p>

      <p v-if="note" class="form-hint" role="status">{{ note }}</p>

      <h2 class="secret-list-title">{{ t('secrets.listTitle') }}</h2>
      <p v-if="loadingSecrets" class="form-hint">{{ t('secrets.loading') }}</p>
      <p v-else-if="secretsError" class="form-error" role="alert">{{ secretsError }}</p>
      <p v-else-if="secrets && secrets.length === 0" class="form-hint">{{ t('secrets.empty') }}</p>
      <ul v-else-if="secrets" class="secret-list">
        <li v-for="name in secrets" :key="name" class="secret-item">
          <span class="secret-name mono">{{ name }}</span>
          <button
            type="button"
            class="btn-secondary secret-delete"
            name="secret-delete"
            :disabled="deletingName === name"
            @click="deleteSecret(name)"
          >
            {{ t('secrets.delete') }}
          </button>
        </li>
      </ul>
    </template>
  </div>
</template>
