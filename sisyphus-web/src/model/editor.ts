// 编辑器辅助（票 B4-T8，ADR-0009/0020）：消费生成的 `pipeline.ts` 类型与
// `validate.ts` 校验，提供编辑器装配用的纯函数——新建脚手架、保存载荷清洗、
// 错误清单按字段路径定位。
//
// 本文件**手写**（非 codegen 产物）：不存类型/校验规则事实——规则单一事实源
// 仍在 `pipeline.ts`/`validate.ts`（生成）与 `sisyphus-model`（Rust，票 B4-T7
// 对账管线锚定）。改 model 后跑 `cargo run -p sisyphus-codegen -- gen` 重生
// 生成物；本文件随生成物的类型变化由 vue-tsc 兜底（类型错即编译红）。

import type { Pipeline, Stage, Job, Step } from './pipeline'

/** 新建 pipeline 脚手架：空参数/env/stages（合法——空定义可通过校验，
 *  `validatePipeline` 对空 stages 无错）。名取路由 pipeline 段——server 以
 *  路径段为存储键，body `name` 不参与键（ADR-0009 定义原样落库）。 */
export function newPipeline(name: string): Pipeline {
  return { name, parameters: [], env: [], stages: [] }
}

/** 保存载荷：剥离 server 独占的 `revision`（每次保存 server 写自己的 revision，
 *  body 内残留的 revision 是过期元数据，原样回传会误导「当前版本」语义）。
 *  其余字段原样提交（ADR-0009：定义以 model JSON 形态往返，schema 不解析内部）。 */
export function toSavePayload(pipeline: Pipeline): Pipeline {
  const { revision: _revision, ...rest } = pipeline
  return rest
}

/** 新建阶段脚手架：空 when + 空 jobs（合法）。 */
export function newStage(name: string): Stage {
  return { name, jobs: [] }
}

/** 新建任务脚手架：必填 `steps` 为空数组（合法），其余可选字段缺省（serde
 *  `skip_none`/`skip_if_empty` 形态——保存时空字段省略，与 model 一致）。 */
export function newJob(name: string): Job {
  return { name, steps: [] }
}

/** 新建 shell 步骤（command 空——保存时 `shell_command_empty` 校验拦下，
 *  引导用户填命令；shell 为 null = 用默认解释器，ADR-0006）。 */
export function newShellStep(): Step {
  return { type: 'shell', config: { command: '', shell: null } }
}

/** 新建 checkout 步骤（submodules 缺省 = model 默认开，ADR-0016；显式 false 才关）。 */
export function newCheckoutStep(): Step {
  return { type: 'checkout', config: {} }
}

/** 错误清单按字段路径前缀过滤（编辑器字段定位：把校验 `path` 前缀匹配到具体
 *  字段。服务端 422 的 `ValidationIssue.path` 与本地 `ValidationError.path` 同形，
 *  同一规则源 → 一致定位，ADR-0009）。
 *
 *  精确前缀匹配（`path === prefix` 或 `path` 以 `prefix + '.'` 起首）：避免
 *  `jobs[1]` 误命中 `jobs[10]`（`.` 分隔符阻断跨级越界）。
 *
 *  泛型 `E`：只要求行带 `path`，使本地 `ValidationError`（带 `code`）与服务端
 *  `ValidationIssue`（{path,message}）同一函数消费、各自返回原类型。 */
export function errorsForField<E extends { path: string }>(
  errors: E[],
  prefix: string,
): E[] {
  return errors.filter((e) => e.path === prefix || e.path.startsWith(prefix + '.'))
}

/** 阶段级错误（仅阶段 when：path 为裸 `stages[si]`，不含 `.jobs` 子级——
 *  `errorsForField` 的前缀匹配会把整阶段子任务错也纳入，故阶段级用精确匹配）。 */
export function errorsForStage<E extends { path: string }>(
  errors: E[],
  stageIndex: number,
): E[] {
  const p = `stages[${stageIndex}]`
  return errors.filter((e) => e.path === p)
}

/** 任务级错误（该任务本身 + 其下所有子字段：when / steps / env / caches / 产物）。
 *  供轨道 chip 的「状态色边」与表单内联定位共用。 */
export function errorsForJob<E extends { path: string }>(
  errors: E[],
  stageIndex: number,
  jobIndex: number,
): E[] {
  return errorsForField(errors, `stages[${stageIndex}].jobs[${jobIndex}]`)
}

// textarea ↔ string[] 互转（标签 / 缓存 paths·files / enum 候选项共用：每行一条，
// 丢弃空行）。四处调用方共用同一形态，提取避免漂移（Fowler Duplicated Code）。
export function linesToText(lines: string[]): string {
  return lines.join('\n')
}
export function textToLines(text: string): string[] {
  return text.split('\n').filter((s) => s !== '')
}

/** 带边界守卫的数组交换（阶段/任务/步骤重排共用：越界不动，非拖拽）。 */
export function swap<T>(arr: T[], i: number, j: number): void {
  if (i < 0 || j < 0 || i >= arr.length || j >= arr.length) return
  const tmp = arr[i]!
  arr[i] = arr[j]!
  arr[j] = tmp
}
