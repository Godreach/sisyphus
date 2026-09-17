import { onBeforeUnmount, ref, watch } from 'vue'

import { projectsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ProjectResponse } from '@/api/types'
import { useAuthStore } from '@/stores/auth'

/** Creation permissions come from the server policy, never member/credential probes. */
export function useManageableProjects(scope: () => string) {
  const auth = useAuthStore()
  const projects = ref<ProjectResponse[]>([])
  const status = ref<'idle' | 'loading' | 'ready' | 'error'>('idle')
  const error = ref('')
  let requestSequence = 0

  async function reload(): Promise<void> {
    const sequence = ++requestSequence
    projects.value = []
    error.value = ''
    if (!scope()) {
      status.value = 'idle'
      return
    }
    status.value = 'loading'
    try {
      const result = await projectsApi.list({ permission: 'admin' })
      if (sequence !== requestSequence) return
      projects.value = result
      status.value = 'ready'
    } catch (err) {
      if (sequence !== requestSequence) return
      error.value = describeSubmitError(err)
      status.value = 'error'
    }
  }

  watch([scope, () => auth.user?.username, () => auth.user?.isAdmin], reload, { immediate: true })
  onBeforeUnmount(() => { ++requestSequence })
  return { projects, status, error, reload }
}
