// Pipeline 定义保存校验——sisyphus-model `validate.rs` 的 TS 端口（票 B4-T7，ADR-0009）。
//
// 编辑器与前端实时校验与 model 单一事实源对账：坏定义在保存前报错。非短路，返回全部
// 错误（与 Rust `validate()` 同）。校验码 `code` 为稳定规则身份（生成的 `codes.ts` 镜像），
// 前端对账测试据此与 model 结论对账；`path` 与 Rust 同形（编辑器本地错与服务端错显示一致）。
//
// 注：message 文案与 Rust 中文措辞同形但不参与对账（避免耦合措辞微调）；对账只比 code 与 valid。

import type { Pipeline, Parameter, Job, CacheSpec, Step } from './pipeline'
import type { ValidationCode } from './codes'
import { isValidWhen } from './when'

/** 校验错误：`path` 字段定位 + 规则码 + 人读信息（与 model `ValidationError` 同义）。 */
export interface ValidationError {
  /** 字段定位路径（如 `stages[0].jobs[1].caches[0].key`），与 Rust 同形。 */
  path: string
  /** 规则码（稳定身份，对账据此）。 */
  code: ValidationCode
  /** 人读错误描述（不参与对账）。 */
  message: string
}

/** 校验 Pipeline 定义；返回全部错误（非短路，与 Rust `validate()` 同）。空数组 = 合法。 */
export function validatePipeline(pipeline: Pipeline): ValidationError[] {
  const errors: ValidationError[] = []

  // Pipeline 级：必填参数必须带默认值（R1）；enum 必须有候选项（R2）。
  pipeline.parameters.forEach((p: Parameter, i: number) => {
    const path = `parameters[${i}]`
    if (p.required && p.default === undefined) {
      errors.push({
        path: `${path}.${p.name}.required`,
        code: 'required_parameter_default',
        message: '必填参数必须带默认值（ADR-0006：所有触发方式统一「默认值，手动触发可覆盖」）',
      })
    }
    if (p.type === 'enum' && (!p.choices || p.choices.length === 0)) {
      errors.push({
        path: `${path}.${p.name}.choices`,
        code: 'enum_choices',
        message: 'enum 类型参数必须提供候选项',
      })
    }
  })

  // 阶段/任务：when 语法 + 禁 SISY_WORKSPACE（R3/R4）+ 任务级规则。
  pipeline.stages.forEach((stage, si: number) => {
    validateWhen(errors, `stages[${si}]`, stage.when)
    stage.jobs.forEach((job: Job, ji: number) => {
      const jobPath = `stages[${si}].jobs[${ji}]`
      validateWhen(errors, `${jobPath}.when`, job.when)
      validateJob(errors, jobPath, job)
    })
  })

  return errors
}

// when 校验：禁 SISY_WORKSPACE（R3）+ 受限语法（R4）。两者皆可触发（非短路）。
function validateWhen(
  errors: ValidationError[],
  path: string,
  source: string | undefined,
): void {
  if (source === undefined) {
    return
  }
  if (source.includes('${SISY_WORKSPACE}')) {
    errors.push({
      path,
      code: 'when_workspace',
      message: 'when 表达式禁用 ${SISY_WORKSPACE}（Agent 侧才可知其值，Server 端无法求值）',
    })
  }
  if (!isValidWhen(source)) {
    errors.push({ path, code: 'when_syntax', message: 'when 表达式语法不合法' })
  }
}

// 任务级规则：步骤 when/空命令（R4/R5）+ 容器 image（R6）+ env/机密冲突（R7）+
// 产物上传（R8/R9）+ 缓存（R10–R14）。
function validateJob(errors: ValidationError[], path: string, job: Job): void {
  // 步骤级 when（R4）+ shell 命令非空（R5）。
  job.steps.forEach((step: Step, si: number) => {
    validateWhen(errors, `${path}.steps[${si}].when`, step.config.when)
    if (step.type === 'shell' && step.config.command.trim() === '') {
      errors.push({
        path: `${path}.steps[${si}].command`,
        code: 'shell_command_empty',
        message: 'shell 步骤命令不能为空',
      })
    }
  })

  // 执行环境：容器 image 非空（R6）。
  const env = job.exec_env
  if (env !== undefined && env.type === 'container' && env.config.image.trim() === '') {
    errors.push({
      path: `${path}.exec_env.image`,
      code: 'container_image_empty',
      message: '容器执行环境必须指定 image',
    })
  }

  // env 键与机密名冲突（R7）。
  const secretSet = new Set<string>(job.secrets ?? [])
  ;(job.env ?? []).forEach((e) => {
    if (secretSet.has(e.name)) {
      errors.push({
        path: `${path}.env.${e.name}`,
        code: 'env_secret_collision',
        message: '任务 env 键与机密名冲突（ADR-0015：机密经 env 注入，键名冲突）',
      })
    }
  })

  // 产物上传：name/path 非空（R8）+ 相对路径（R9）。
  ;(job.artifact_uploads ?? []).forEach((u, ui: number) => {
    if (u.name.trim() === '' || u.path.trim() === '') {
      errors.push({
        path: `${path}.artifact_uploads[${ui}]`,
        code: 'artifact_upload_empty',
        message: '产物上传需指定非空的 name 与 workspace 相对路径',
      })
    }
    if (isAbsolute(u.path)) {
      errors.push({
        path: `${path}.artifact_uploads[${ui}].path`,
        code: 'artifact_upload_absolute',
        message: '产物上传路径必须是 workspace 相对路径，不支持绝对路径',
      })
    }
  })

  // 缓存声明（R10–R14）。
  ;(job.caches ?? []).forEach((cache: CacheSpec, ci: number) => {
    validateCache(errors, `${path}.caches[${ci}]`, cache)
  })
}

// 缓存规则：key 非空（R10）/ 长度上限（R11）/ 禁 SISY_WORKSPACE（R12）+
// paths 相对路径（R13）/ files 禁 glob（R14）。
function validateCache(errors: ValidationError[], path: string, cache: CacheSpec): void {
  if (cache.key.trim() === '') {
    errors.push({ path: `${path}.key`, code: 'cache_key_empty', message: '缓存 key 不能为空' })
  }
  if (byteLength(cache.key) > 255) {
    errors.push({ path: `${path}.key`, code: 'cache_key_too_long', message: '缓存 key 长度超过上限 255' })
  }
  if (cache.key.includes('${SISY_WORKSPACE}')) {
    errors.push({
      path: `${path}.key`,
      code: 'cache_key_workspace',
      message: '缓存 key 禁用 ${SISY_WORKSPACE}（per-Agent 值会让 key 永不命中）',
    })
  }
  cache.paths.forEach((p: string, pi: number) => {
    if (isAbsolute(p) || p.startsWith('..')) {
      errors.push({
        path: `${path}.paths[${pi}]`,
        code: 'cache_path_not_relative',
        message: '缓存 paths 仅允许 workspace 相对路径',
      })
    }
  })
  cache.files.forEach((f: string, fi: number) => {
    if (f.includes('*') || f.includes('?')) {
      errors.push({
        path: `${path}.files[${fi}]`,
        code: 'cache_files_glob',
        message: '缓存 files 仅支持精确路径，不支持 glob',
      })
    }
  })
}

// 绝对路径判定（与 Rust `is_absolute` 同：`/` 或 `\` 起首）。
function isAbsolute(p: string): boolean {
  return p.startsWith('/') || p.startsWith('\\')
}

// UTF-8 字节长度（与 Rust `str::len` 同——Rust 按 UTF-8 字节计，TS `.length` 按
// UTF-16 码元计，多字节 key 会二者分叉）。R11 缓存 key 长度上限据此对账。
function byteLength(s: string): number {
  return new TextEncoder().encode(s).length
}
