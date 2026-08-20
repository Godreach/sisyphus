// 构建详情 store（票 B4-T4，ADR-0006/0008/0013）：构建详情 + pipeline 定义
// 的加载与热刷新（进行中构建轮询）、触发/取消/重跑的提交动作。
//
// - 详情（`GET .../builds/{number}`）加载后，构建非终态（queued/running）
//   时按 `POLL_MS` 轮询刷新——阶段/任务卡随调度推进更新；终态停轮询。
// - pipeline 定义（`GET .../pipelines/{pipeline}`）并行加载：触发对话框的
//   参数覆盖表单（ADR-0006 参数定义）与排队任务「等待匹配 agent：缺标签 X」
//   的等待态展示（ADR-0008；REST 详情不含 waiting_detail，从定义的任务
//   labels 声明派生——退化派生 + 定义缺失时显式标注）。
// - 触发（202 受理）、取消（终态幂等 202）、重跑（from_scratch/from_failed，
//   非失败终态 409）提交后刷新详情；from_scratch 新号由调用侧跳转。
// - 401/403/404/409 以 `ApiError` 上抛，视图侧按 code 分支展示（409 重跑
//   拒绝提示、404 展示「构建不存在」）。

import { ref } from 'vue'
import { defineStore } from 'pinia'

import { buildsApi, artifactsApi, pipelinesApi } from '@/api/client'
import { isLiveStatus } from '@/utils/format'
import type {
  BuildDetailResponse,
  BuildAcceptedResponse,
  BuildArtifactsResponse,
  ModelPipeline,
  RerunBuildRequest,
  TriggerBuildRequest,
} from '@/api/types'

/** 进行中构建的轮询节奏（毫秒）。 */
export const BUILD_POLL_MS = 3000

/** 详情加载状态（视图分支消费）。 */
export type BuildDetailStatus = 'loading' | 'ready' | 'error' | 'not-found'

export const useBuildDetailStore = defineStore('buildDetail', () => {
  const build = ref<BuildDetailResponse | null>(null)
  const definition = ref<ModelPipeline | null>(null)
  /** 已上传产物清单（详情页产物区：声明 × 已上传比对）。 */
  const artifacts = ref<BuildArtifactsResponse['items']>([])
  const status = ref<BuildDetailStatus>('loading')
  const errorMessage = ref('')

  /** 轮询定时器句柄（组件卸载时经 dispose 清理）。 */
  let timer: ReturnType<typeof setInterval> | null = null

  /**
   * 从 model Pipeline 定义解析任务声明的 labels（排队等待态展示用）。
   *
   * 注意（退化派生）：REST 构建详情不含 per-job 的 labels/waiting_detail
   * 字段（`JobViewDto`），而 ADR-0006 的「构建快照」才是构建 #N 当时定义的
   * 唯一事实源——本处从**当前** pipeline 定义派生，定义在构建后若被编辑会与
   * 快照漂移。这是 B4「纯前端消费既有契约 + 缺字段走退化态」纪律下的可用
   * 兜底：定义缺失时视图侧显式标注退化（`waitingDegraded`），精确的
   * 快照内 labels 展示留待 server 详情端点补齐后的后续票。
   */
  function jobLabels(stageIndex: number, jobName: string): string[] {
    const stage = definition.value?.stages?.[stageIndex]
    const job = stage?.jobs?.find((j) => j.name === jobName)
    return job?.labels ?? []
  }

  /** 从 model Pipeline 定义解析任务声明的产物上传（产物区声明展示用）。 */
  function jobArtifactUploads(stageIndex: number, jobName: string) {
    const stage = definition.value?.stages?.[stageIndex]
    const job = stage?.jobs?.find((j) => j.name === jobName)
    return job?.artifact_uploads ?? []
  }

  /** 按名查已上传产物（产物区下载链接接上，票 #74）：未上传返回 null
   *  （视图展示占位）；同名重传以列表最新为准。 */
  function uploadedArtifact(name: string) {
    return artifacts.value.find((a) => a.name === name) ?? null
  }

  async function load(
    project: string,
    pipeline: string,
    number: number,
  ): Promise<void> {
    status.value = 'loading'
    errorMessage.value = ''
    try {
      const [detail, defResp, arts] = await Promise.all([
        buildsApi.detail(project, pipeline, number),
        // 定义加载失败不阻塞详情（404 时无定义——产物/等待态退化为空）。
        pipelinesApi.getDefinition(project, pipeline).catch(() => null),
        // 产物列表失败不阻塞详情（产物区退化为声明占位）。
        artifactsApi.list(project, pipeline, number).catch(() => null),
      ])
      build.value = detail
      definition.value =
        (defResp?.definition as ModelPipeline | undefined) ?? null
      artifacts.value = arts?.items ?? []
      status.value = 'ready'
      schedulePoll(project, pipeline, number)
    } catch (err) {
      status.value = err instanceof Error && 'status' in err && (err as { status?: number }).status === 404
        ? 'not-found'
        : 'error'
      errorMessage.value =
        err instanceof Error ? err.message : '构建详情加载失败'
      build.value = null
    }
  }

  /** 刷新详情（轮询与提交后共用；终态停轮询；产物列表随刷新同步——构建
   *  推进中任务陆续上传产物，页面须跟上传态）。 */
  async function refresh(
    project: string,
    pipeline: string,
    number: number,
  ): Promise<void> {
    if (status.value === 'not-found') return
    try {
      const [detail, arts] = await Promise.all([
        buildsApi.detail(project, pipeline, number),
        artifactsApi.list(project, pipeline, number).catch(() => null),
      ])
      build.value = detail
      artifacts.value = arts?.items ?? []
      status.value = 'ready'
      schedulePoll(project, pipeline, number)
    } catch {
      // 刷新失败保持现有详情（下一次轮询再试），不打断阅读。
    }
  }

  /** 进行中构建启动/续期轮询；终态停止。 */
  function schedulePoll(project: string, pipeline: string, number: number): void {
    if (timer) {
      clearInterval(timer)
      timer = null
    }
    if (build.value && isLiveStatus(build.value.status)) {
      timer = setInterval(() => {
        void refresh(project, pipeline, number)
      }, BUILD_POLL_MS)
    }
  }

  /** 触发构建（runner 档）：参数覆盖 + 分支/commit，202 受理返回新构建号。 */
  async function trigger(
    project: string,
    pipeline: string,
    req: TriggerBuildRequest,
  ): Promise<BuildAcceptedResponse> {
    return buildsApi.trigger(project, pipeline, req)
  }

  /** 取消构建（runner 档）：终态幂等 202。 */
  async function cancel(
    project: string,
    pipeline: string,
    number: number,
  ): Promise<BuildAcceptedResponse> {
    const accepted = await buildsApi.cancel(project, pipeline, number)
    await refresh(project, pipeline, number)
    return accepted
  }

  /** 重跑构建：from_scratch 新号 / from_failed 同号 attempt+1；非失败终态
   *  from_failed 抛 409（ApiError）。 */
  async function rerun(
    project: string,
    pipeline: string,
    number: number,
    req: RerunBuildRequest,
  ): Promise<BuildAcceptedResponse> {
    const accepted = await buildsApi.rerun(project, pipeline, number, req)
    if (req.mode === 'from_failed') {
      await refresh(project, pipeline, number)
    }
    return accepted
  }

  /** 组件卸载清理：停止轮询、清态（下次进入重新加载）。 */
  function dispose(): void {
    if (timer) {
      clearInterval(timer)
      timer = null
    }
    build.value = null
    definition.value = null
    artifacts.value = []
    status.value = 'loading'
    errorMessage.value = ''
  }

  return {
    build,
    definition,
    artifacts,
    status,
    errorMessage,
    jobLabels,
    jobArtifactUploads,
    uploadedArtifact,
    load,
    refresh,
    trigger,
    cancel,
    rerun,
    dispose,
  }
})
