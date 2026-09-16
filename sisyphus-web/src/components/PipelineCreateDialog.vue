<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute, useRouter } from 'vue-router'
import {
  NAlert,
  NButton,
  NEmpty,
  NFormItem,
  NInput,
  NModal,
  NSelect,
  NSkeleton,
  type SelectOption,
} from 'naive-ui'

import { pipelinesApi, projectsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { ProjectResponse } from '@/api/types'
import { useAuthStore } from '@/stores/auth'
import { returnSourceQuery } from '@/utils/returnSource'

const props = defineProps<{
  show: boolean
  lockedProject?: string
  focusSelector?: string
}>()

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const auth = useAuthStore()

const status = ref<'idle' | 'loading' | 'ready' | 'error'>('idle')
const manageableProjects = ref<ProjectResponse[]>([])
const selectedProject = ref<string | null>(null)
const pipelineName = ref('')
const projectError = ref('')
const nameError = ref('')
const submitError = ref('')
const submitting = ref(false)

const projectOptions = computed(() =>
  manageableProjects.value.map((project) => ({ label: project.name, value: project.name })),
)

function filterProjectOption(pattern: string, option: SelectOption): boolean {
  return String(option.label ?? '').toLocaleLowerCase().includes(pattern.trim().toLocaleLowerCase())
}

function pipelinesQueryWithoutCreate(): Record<string, string | string[]> {
  const query: Record<string, string | string[]> = {}
  for (const [key, value] of Object.entries(route.query)) {
    if (key === 'create' || value == null) continue
    query[key] = Array.isArray(value)
      ? value.filter((item): item is string => item != null)
      : value
  }
  return query
}

function resetForm(): void {
  selectedProject.value = props.lockedProject ?? null
  pipelineName.value = ''
  projectError.value = ''
  nameError.value = ''
  submitError.value = ''
  submitting.value = false
}

async function loadManageableProjects(): Promise<void> {
  status.value = 'loading'
  submitError.value = ''
  try {
    manageableProjects.value = await projectsApi.list({ permission: 'admin' })
    status.value = 'ready'
  } catch (err) {
    manageableProjects.value = []
    submitError.value = describeSubmitError(err)
    status.value = 'error'
  }
}

async function close(): Promise<void> {
  if (!props.show || submitting.value) return
  await router.replace({ path: route.path, query: pipelinesQueryWithoutCreate() })
  await nextTick()
  restoreFocus()
}

function restoreFocus(): void {
  ;(
    document.querySelector(
      props.focusSelector ?? '[data-testid="topbar-cta"]',
    ) as HTMLElement | null
  )?.focus()
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key !== 'Escape' || !props.show || submitting.value) return
  event.preventDefault()
  void close()
}

async function openNewProjectFlow(): Promise<void> {
  if (submitting.value) return
  await router.push({ name: 'projects', query: { create: '1' } })
}

function validateName(name: string): string {
  if (name === '') return t('plines.createNameRequired')
  if (name === '.' || name === '..' || /[\\/\u0000-\u001f\u007f]/u.test(name)) {
    return t('plines.createNameInvalid')
  }
  return ''
}

async function createAndEdit(): Promise<void> {
  if (submitting.value) return
  const project = selectedProject.value
  const name = pipelineName.value.trim()
  projectError.value = project == null ? t('plines.createProjectRequired') : ''
  nameError.value = validateName(name)
  submitError.value = ''
  if (projectError.value || nameError.value || project == null) return

  submitting.value = true
  try {
    try {
      await pipelinesApi.getDefinition(project, name)
      nameError.value = t('plines.createNameExists')
      return
    } catch (err) {
      if (!(err instanceof ApiError) || err.status !== 404) throw err
    }

    const refreshed = await projectsApi.list({ permission: 'admin' })
    manageableProjects.value = refreshed
    if (!refreshed.some((candidate) => candidate.name === project)) {
      projectError.value = t('plines.createProjectUnavailable')
      return
    }

    await router.replace({
      name: 'pipeline-edit',
      params: { name: project, pipeline: name },
      query: { create: '1', ...returnSourceQuery(route, router, ['create']) },
    })
  } catch (err) {
    submitError.value = describeSubmitError(err)
  } finally {
    submitting.value = false
  }
}

watch(
  () => props.show,
  async (show, previous) => {
    if (show) {
      resetForm()
      await loadManageableProjects()
      return
    }
    if (previous) {
      await nextTick()
      setTimeout(restoreFocus, 0)
    }
  },
  { immediate: true },
)

onMounted(() => document.addEventListener('keydown', onKeydown))
onBeforeUnmount(() => document.removeEventListener('keydown', onKeydown))
</script>

<template>
  <n-modal
    :show="show"
    preset="card"
    :title="t('plines.newPipeline')"
    style="width: min(520px, calc(100vw - 32px))"
    :bordered="false"
    :mask-closable="!submitting"
    :close-on-esc="false"
    @update:show="(value: boolean) => { if (!value) void close() }"
    @after-leave="restoreFocus"
  >
    <div data-testid="new-pipeline-dialog" class="create-pipeline-dialog">
      <template v-if="status === 'loading'">
        <div data-testid="create-projects-loading">
          <n-skeleton text :repeat="3" height="40px" />
        </div>
      </template>

      <n-alert
        v-else-if="status === 'error'"
        type="error"
        :title="submitError || t('plines.createProjectsLoadError')"
        role="alert"
        data-testid="create-projects-error"
      >
        <n-button
          secondary
          type="primary"
          data-testid="create-projects-retry"
          @click="loadManageableProjects"
        >
          {{ t('plines.retry') }}
        </n-button>
      </n-alert>

      <n-empty
        v-else-if="status === 'ready' && manageableProjects.length === 0"
        :description="t('plines.createProjectsEmpty')"
        data-testid="create-projects-empty"
      >
        <template #extra>
          <n-button
            v-if="auth.user?.isAdmin"
            type="primary"
            data-testid="create-new-project"
            @click="openNewProjectFlow"
          >
            {{ t('projects.newProject') }}
          </n-button>
          <p v-else class="form-hint">{{ t('plines.createProjectsEmptyHint') }}</p>
        </template>
      </n-empty>

      <template v-else-if="status === 'ready'">
        <n-form-item
          :label="t('plines.createProjectLabel')"
          :validation-status="projectError ? 'error' : undefined"
        >
          <div
            v-if="lockedProject"
            class="locked-project"
            data-testid="new-pipeline-project-locked"
          >
            {{ lockedProject }}
          </div>
          <n-select
            v-else
            :value="selectedProject"
            :options="projectOptions"
            :filter="filterProjectOption"
            filterable
            clearable
            data-testid="new-pipeline-project"
            :placeholder="t('plines.createProjectPlaceholder')"
            @update:value="(value: string | null) => { selectedProject = value; projectError = '' }"
          />
        </n-form-item>
        <p
          v-if="projectError"
          class="create-field-error"
          role="alert"
          data-testid="new-pipeline-project-error"
        >
          {{ projectError }}
        </p>

        <n-form-item
          :label="t('projects.newPipelineName')"
          :validation-status="nameError ? 'error' : undefined"
        >
          <n-input
            v-model:value="pipelineName"
            :input-props="{ name: 'new-pipeline-name', autocomplete: 'off' }"
            :placeholder="t('projects.newPipelinePlaceholder')"
            @update:value="nameError = ''"
            @keyup.enter="createAndEdit"
          />
        </n-form-item>
        <p
          v-if="nameError"
          class="create-field-error"
          role="alert"
          data-testid="new-pipeline-name-error"
        >
          {{ nameError }}
        </p>

        <n-alert
          v-if="submitError"
          type="error"
          :title="submitError"
          role="alert"
          data-testid="new-pipeline-submit-error"
        />
      </template>

      <div class="create-pipeline-actions">
        <n-button
          :disabled="submitting"
          data-testid="new-pipeline-cancel"
          @click="close"
        >
          {{ t('common.cancel') }}
        </n-button>
        <n-button
          v-if="status === 'ready' && manageableProjects.length > 0"
          type="primary"
          :loading="submitting"
          :disabled="submitting"
          data-testid="new-pipeline-create"
          @click="createAndEdit"
        >
          {{ t('projects.newPipelineCreate') }}
        </n-button>
      </div>
    </div>
  </n-modal>
</template>

<style scoped>
.create-pipeline-dialog {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.create-pipeline-dialog :deep(.n-form-item) {
  margin-bottom: 0;
}

.create-field-error {
  margin: -6px 0 0;
  color: var(--sisy-color-danger);
  font-size: 12px;
}

.create-pipeline-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 4px;
}

.locked-project {
  width: 100%;
  padding: 8px 12px;
  border: 1px solid var(--sisy-color-border);
  border-radius: 6px;
  background: var(--sisy-color-bg);
  color: var(--sisy-color-text);
}
</style>
