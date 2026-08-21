#!/usr/bin/env node
// Headless 冒烟（票 B4-T9 / #71 AC）：构建产物 + 预览端口点击走通 12 页主路径。
//
// 形态基准：原型 `prototype/web-ui-ia` 分支 `web/scripts/smoke.mjs`（playwright
// + 预览端口点击）。本脚本是其真实前端版：`vite preview` 伺服 `dist/` 构建产物，
// playwright 在真实浏览器里驱动 SPA——测的是「构建产物在真实浏览器里能启动、
// 12 条路由能解析渲染、i18n 能切、无运行期崩溃」，这是 vitest（jsdom、源码
// 变换）覆盖不到的构建/路由/历史 API 面。
//
// 后端由 playwright `page.route` 在浏览器侧拦截 `/api/v1/**` 注入 mock（替代
// 原型的 data-mock 层）：不拉真后端——smoke 的职责是浏览器渲染面，数据往返由
// Rust `web_handshake.rs` 进程内 oneshot 兜底（那条缝守「不起 socket/进程」
// 纪律；本冒烟为真实浏览器驱动，起预览端口与浏览器进程是原型同款形态）。两个
// 上下文：
//   - authed：mock `/auth/me`→admin、`/auth/setup`→404（非空库，引导已完成），
//     走 12 条受保护路由，断言页标题渲染。
//   - guest：mock `/auth/me`→401（未登录）、`/auth/setup`→422（空库，需引导），
//     走受保护页触发守卫 setup-needed 重定向到 `/setup`，断言引导页渲染；
//     另直访 `/login` 断言登录表单渲染。
//
// 用法：先 `npm run build`（产物进 dist/），再 `npm run smoke`。
// CI：`npm run build` 后接 `npm run smoke`（见 .github/workflows/ci.yml）。
// 本地跳过 chromium 下载：`SMOKE_CHROMIUM_EXECUTABLE` 指向系统 Chrome/Edge。

import { existsSync } from 'node:fs'
import { spawn } from 'node:child_process'
import { createRequire } from 'node:module'
import http from 'node:http'
import { setTimeout as sleep } from 'node:timers/promises'

import { chromium } from 'playwright'

const require = createRequire(import.meta.url)
const ROOT = require('path').resolve(import.meta.dirname, '..')
const DIST_INDEX = require('path').join(ROOT, 'dist', 'index.html')
const PORT = 4173
const BASE = `http://localhost:${PORT}`

const results = []
const ok = (name, pass, extra = '') => results.push({ name, pass, extra })

// --- mock 数据（最小可用形态：页标题与主结构能渲染，不测数据细节）----------
const me = { username: 'admin', is_admin: true }
const project = {
  id: 1,
  name: 'demo',
  scm_type: 'git',
  scm_url: 'https://example.com/demo',
  default_branch: 'main',
  created_at: 0,
  updated_at: 0,
}
const agent = {
  name: 'linux-1',
  online: true,
  disabled: false,
  system_labels: ['sisyphus/os=linux'],
  custom_labels: [],
  max_concurrency: 2,
  active_jobs: 0,
  last_seen_at: null,
  disk_usage: null,
  created_at: 0,
  updated_at: 0,
}
const pipelineDef = {
  definition: {
    name: 'release',
    parameters: [],
    env: [],
    notification: null,
    stages: [
      {
        name: 'build',
        when: null,
        jobs: [
          {
            name: 'compile',
            exec_env: null,
            labels: ['sisyphus/os=linux'],
            when: null,
            env: [],
            allow_failure: false,
            retry_count: 0,
            timeout_minutes: 0,
            artifact_uploads: [],
            artifact_downloads: [],
            caches: [],
            secrets: [],
            steps: [{ type: 'shell', config: { command: 'echo hi', when: null } }],
          },
        ],
      },
    ],
    revision: null,
  },
  revision: 1,
  operator: 'admin',
  updated_at: 0,
}
const buildDetail = {
  number: 1,
  pipeline_name: 'release',
  status: 'succeeded',
  trigger: 'manual',
  trigger_by: 'admin',
  attempt: 1,
  started_at: 0,
  finished_at: 1000,
  cancelled_at: null,
  elapsed_ms: 1000,
  stages: [
    {
      index: 0,
      name: 'build',
      jobs: [
        {
          name: 'compile',
          status: 'succeeded',
          attempt: 1,
          started_at: 0,
          finished_at: 1000,
          exit_code: 0,
          allow_failure: false,
          detail: null,
          agent_id: 1,
        },
      ],
    },
  ],
}
const buildList = { items: [], total: 0, page: 1, limit: 20 }

// 升级包（升级页表格区渲染用——占位态消除：mock 回非空包，断言 upgrade-table 出现）。
const upgradePackage = {
  package_name: 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz',
  version: { major: 1, minor: 0, patch: 0 },
  target_os: 'linux',
  target_arch: 'x86_64',
  size: 1234,
  sha256: 'deadbeef',
  created_at: 0,
}
// SCM 探测（test-connection 动作用：scm-probe 回 head、scm-branches 回默认分支）。
const scmProbe = { head: 'abc123deadbeef' }
const scmBranches = {
  branches: [
    { name: 'main', head: 'abc123deadbeef' },
    { name: 'dev', head: 'def456' },
  ],
  default_branch: 'main',
}
// 概览快照（overview stat 卡渲染用——全卡真值，无退化态）。
const overviewSnapshot = {
  queue_depth: 0,
  queue_reasons: [],
  agents_online: 1,
  agents_total: 1,
  slots_used: 0,
  slots_total: 2,
  builds_terminal: { succeeded: 1, failed: 0, cancelled: 0, timeout: 0 },
  artifact_bytes: 1024,
  log_bytes: 2048,
  alerts: { has_no_match: false, has_offline_agent: false, has_draining_incompatible: false },
  recent_builds: [
    {
      project: 'demo',
      pipeline: 'release',
      number: 1,
      status: 'succeeded',
      trigger: 'manual',
      started_at: 0,
      finished_at: 1000,
    },
  ],
}

/** `/api/v1/**` 拦截器工厂：authed 决定会话态与空库判定。
 *  - authed=true：`/auth/me`→200 admin，`/auth/setup`→404（非空库，引导已完成，
 *    受保护页守卫不再探测 setup）。
 *  - authed=false（空库）：`/auth/me`→401（未登录），`/auth/setup`→422
 *    （空库 + 非法输入探测，引导需要——受保护页守卫据此重定向 `/setup`，
 *    ADR-0010）。 */
function mockApi(authed) {
  return async (route) => {
    const url = new URL(route.request().url())
    const p = url.pathname
    const m = route.request().method()
    const json = (status, body) =>
      route.fulfill({
        status,
        contentType: 'application/json',
        body: JSON.stringify(body),
      })
    if (p === '/api/v1/auth/me') {
      return authed
        ? json(200, me)
        : json(401, { code: 'UNAUTHORIZED', message: 'no session' })
    }
    if (p === '/api/v1/auth/setup') {
      return authed
        ? json(404, { code: 'NOT_FOUND', message: 'setup done' })
        : json(422, {
            code: 'VALIDATION_FAILED',
            message: '凭据输入校验失败',
            detail: { errors: [{ path: 'username', message: 'x' }] },
          })
    }
    if (p === '/api/v1/auth/login' || p === '/api/v1/auth/logout') {
      return json(200, me)
    }
    // SCM 探测（test-connection 动作：scm-probe / scm-branches）。
    if (m === 'POST' && p === '/api/v1/projects/scm-probe') return json(200, scmProbe)
    if (m === 'POST' && p === '/api/v1/projects/scm-branches') return json(200, scmBranches)
    if (m === 'GET') {
      if (p === '/api/v1/projects') return json(200, [project])
      if (p === '/api/v1/projects/demo') return json(200, project)
      if (p === '/api/v1/projects/demo/members') return json(200, [])
      if (p === '/api/v1/projects/demo/secrets') return json(200, [])
      if (p === '/api/v1/projects/demo/pipelines/release') return json(200, pipelineDef)
      if (p === '/api/v1/projects/demo/pipelines/release/builds') return json(200, buildList)
      if (p === '/api/v1/projects/demo/pipelines/release/builds/1') return json(200, buildDetail)
      if (p === '/api/v1/agents') return json(200, [agent])
      if (p === '/api/v1/agents/linux-1') return json(200, agent)
      if (p === '/api/v1/upgrade-packages') return json(200, [upgradePackage])
      if (p === '/api/v1/overview') return json(200, overviewSnapshot)
      if (p === '/api/v1/audit') return json(200, [])
      if (p === '/api/v1/users') return json(200, [])
      if (p === '/api/v1/users/directory') return json(200, [])
      if (p === '/api/v1/auth/tokens') return json(200, [])
      // 兜底：未知 GET 回空数组（页面以空态渲染，不崩）。
      return json(200, [])
    }
    // 兜底：非 GET 回空对象。
    return json(200, {})
  }
}

// --- 预览服务器生命周期 --------------------------------------------------
function startPreview() {
  return new Promise((resolve, reject) => {
    // shell:true + 单字符串命令（非 args 数组）——跨平台经 shell 解析 `npx`，
    // 规避「shell:true 配 args 数组」的参数不转义告警（DEP0190）。
    const child = spawn('npx vite preview --port 4173 --strictPort', {
      cwd: ROOT,
      shell: true,
      stdio: 'ignore',
    })
    child.on('error', reject)
    child.on('exit', (code) => {
      // 若仍在等待就绪期间退出，即启动失败。
      reject(new Error(`vite preview 异常退出（code=${code}）`))
    })
    resolve(child)
  })
}

async function waitReady(deadline) {
  for (;;) {
    if (Date.now() > deadline) throw new Error('vite preview 未在 15s 内就绪')
    try {
      await new Promise((resolve, reject) => {
        const req = http.get(`${BASE}/`, (res) => {
          res.resume()
          res.on('end', () => resolve(res.statusCode === 200))
        })
        req.on('error', reject)
        req.setTimeout(1000, () => req.destroy(new Error('timeout')))
      })
      return
    } catch {
      await sleep(250)
    }
  }
}

/** 访问路由并断言首个匹配的 h1 文案（SPA 启动 + 路由守卫 + mock 往返完成）。
 *  pageerror 由各 run* 上下文经 page.on('pageerror') 收集并兜底断言，不在此传入。 */
async function visit(page, path, h1Text, name) {
  try {
    await page.goto(`${BASE}${path}`, { waitUntil: 'domcontentloaded' })
    await page.locator('h1', { hasText: h1Text }).first().waitFor({ timeout: 10000 })
    ok(name, true)
  } catch (err) {
    const body = await page.locator('body').innerText().catch(() => '<unreachable>')
    ok(name, false, `${err.message} | body: ${body.slice(0, 160)}`)
  }
}

async function runAuthed(browser) {
  // 钉中文界面：vue-i18n 的 initialLocale() 先读 localStorage 再读
  // navigator.language（本地系统 Chrome 探出 zh-CN，CI chromium 默认 en-US 探出
  // 英文）——断言文案写死中文，不预置则 CI 全红（页面渲染正常、仅语言不对）。
  // addInitScript 在每页首次脚本执行前注入，先于 i18n 装配读 localStorage。
  const context = await browser.newContext()
  await context.addInitScript(() => {
    window.localStorage.setItem('sisyphus.locale', 'zh-CN')
  })
  const page = await context.newPage()
  const errors = []
  page.on('pageerror', (e) => errors.push(String(e)))
  await context.route('**/api/v1/**', mockApi(true))

  // 12 页主路径（zh-CN 源语言，页标题文案见 i18n/locales/zh-CN.json routes.*）。
  await visit(page, '/', '概览', 'overview')
  await visit(page, '/projects', '项目', 'projects')
  await visit(page, '/projects/demo', 'demo', 'project-detail')
  await visit(page, '/projects/demo/pipelines/release', 'release', 'pipeline-edit')
  await visit(page, '/projects/demo/pipelines/release/builds', 'release', 'build-list')
  await visit(
    page,
    '/projects/demo/pipelines/release/builds/1',
    'release #1',
    'build-detail',
  )
  await visit(page, '/agents', 'Agent', 'agents')
  await visit(page, '/agents/linux-1', 'linux-1', 'agent-detail')
  await visit(page, '/admin/secrets', '机密', 'admin-secrets')
  await visit(page, '/admin/audit', '审计日志', 'admin-audit')
  await visit(page, '/admin/upgrade', 'Agent 升级', 'admin-upgrade')
  await visit(page, '/admin/users', '用户', 'admin-users')

  // 关键动作 1：升级页升级包表格渲染（占位态消除——mock 回非空包，
  // upgrade-table 出现且含包名）。B5-T4 起升级端点已交付，页不再退化。
  try {
    await page.goto(`${BASE}/admin/upgrade`, { waitUntil: 'domcontentloaded' })
    await page.locator('h1', { hasText: 'Agent 升级' }).first().waitFor({ timeout: 10000 })
    await page
      .locator('.upgrade-table td', { hasText: 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz' })
      .first()
      .waitFor({ timeout: 5000 })
    ok('admin-upgrade package table renders', true)
  } catch (err) {
    ok('admin-upgrade package table renders', false, err.message)
  }

  // 关键动作 2：项目页测试连接按钮（B5-T3 SCM 真实探测端点已交付）——展开
  // 新建表单 → 填 scm_url → 点测试连接 → 断言 probeMsg（role=status）渲染
  // mock 回的 head + 预填默认分支。
  try {
    await page.goto(`${BASE}/projects`, { waitUntil: 'domcontentloaded' })
    await page.locator('h1', { hasText: '项目' }).first().waitFor({ timeout: 10000 })
    await page.locator('button[name="project-new"]').click()
    await page.locator('input[name="project-url"]').waitFor({ timeout: 5000 })
    await page.locator('input[name="project-url"]').fill('https://example.com/demo')
    await page.locator('button[name="project-test-connection"]').click()
    await page
      .locator('[role="status"]', { hasText: '连接成功，当前 head：abc123deadbeef' })
      .first()
      .waitFor({ timeout: 5000 })
    await page
      .locator('[role="status"]', { hasText: '已预填默认分支：main' })
      .first()
      .waitFor({ timeout: 5000 })
    ok('projects test-connection renders head + prefilled branch', true)
  } catch (err) {
    const body = await page.locator('body').innerText().catch(() => '<unreachable>')
    ok('projects test-connection renders head + prefilled branch', false, `${err.message} | body: ${body.slice(0, 160)}`)
  }

  // 退化标注消除巡检（B5-T9 AC：关键页面不再有退化标注）。B5-T3/T4/T7 已移除
  // 运行态退化（overview 退化卡 / 升级页占位态 / testConnectionUnavailable）；
  // 此巡检为回归护栏——断言概览页渲染真实 stat 卡（非退化态、无 loadError）。
  // 升级页占位态消除由动作 1（upgrade-table 渲染）守、测试连接由动作 2 守。
  try {
    await page.goto(`${BASE}/`, { waitUntil: 'domcontentloaded' })
    await page.locator('h1', { hasText: '概览' }).first().waitFor({ timeout: 10000 })
    await page.locator('.stat-card').first().waitFor({ timeout: 5000 })
    const cardCount = await page.locator('.stat-card').count()
    const hasError = await page.locator('.overview-error').count()
    ok(
      'overview no-degradation (real stat cards, no error)',
      cardCount > 0 && hasError === 0,
      `cards=${cardCount} error=${hasError}`,
    )
  } catch (err) {
    ok('overview no-degradation (real stat cards, no error)', false, err.message)
  }

  // i18n 即时切换：概览页 h1 概览 → Overview → 概览。
  try {
    await page.goto(`${BASE}/`, { waitUntil: 'domcontentloaded' })
    await page.locator('h1', { hasText: '概览' }).first().waitFor({ timeout: 10000 })
    await page.locator('.lang-switch').click()
    await page.locator('h1', { hasText: 'Overview' }).first().waitFor({ timeout: 5000 })
    await page.locator('.lang-switch').click()
    await page.locator('h1', { hasText: '概览' }).first().waitFor({ timeout: 5000 })
    ok('i18n zh→en→zh', true)
  } catch (err) {
    ok('i18n zh→en→zh', false, err.message)
  }

  for (const e of errors) ok(`pageerror(authed): ${e.slice(0, 120)}`, false, e)
  await context.close()
}

async function runGuest(browser) {
  const context = await browser.newContext()
  await context.addInitScript(() => {
    window.localStorage.setItem('sisyphus.locale', 'zh-CN')
  })
  const page = await context.newPage()
  const errors = []
  page.on('pageerror', (e) => errors.push(String(e)))
  await context.route('**/api/v1/**', mockApi(false))

  // 引导流（ADR-0010）：guest 直访受保护页 `/` → 守卫探测 `/auth/setup`→422
  // （空库需引导）→ 重定向 `/setup` → 引导页渲染（应用名 + intro + 三步指示）。
  // 比直接 goto `/setup` 更忠实——实际走 setup-needed 重定向路径。
  try {
    await page.goto(`${BASE}/`, { waitUntil: 'domcontentloaded' })
    await page
      .locator('h1', { hasText: 'sisyphus' })
      .first()
      .waitFor({ timeout: 10000 })
    await expectUrl(page, '/setup', 'setup-needed redirect')
    await page.locator('.setup-intro').waitFor({ timeout: 5000 })
    await page.locator('.setup-steps li').first().waitFor({ timeout: 5000 })
    ok('setup wizard renders (via redirect)', true)
  } catch (err) {
    ok('setup wizard renders (via redirect)', false, err.message)
  }

  // 登录页：应用名标题 + 用户名/密码字段 + 登录按钮（直访公开页）。
  try {
    await page.goto(`${BASE}/login`, { waitUntil: 'domcontentloaded' })
    await page.locator('h1', { hasText: 'sisyphus' }).first().waitFor({ timeout: 10000 })
    await page.locator('input[name="username"]').waitFor({ timeout: 5000 })
    await page.locator('input[name="password"]').waitFor({ timeout: 5000 })
    await page.locator('button[type="submit"]').waitFor({ timeout: 5000 })
    ok('login form renders', true)
  } catch (err) {
    ok('login form renders', false, err.message)
  }

  for (const e of errors) ok(`pageerror(guest): ${e.slice(0, 120)}`, false, e)
  await context.close()
}

/** 断言页面 URL 路径（忽略 query；SPA 客户端路由 push 后比对 pathname）。 */
async function expectUrl(page, expectedPath, label) {
  const actual = new URL(page.url()).pathname
  if (actual !== expectedPath) {
    throw new Error(`${label}: 期望路径 ${expectedPath}，实际 ${actual}（完整 ${page.url()}）`)
  }
}

async function main() {
  if (!existsSync(DIST_INDEX)) {
    console.error(`构建产物缺失：${DIST_INDEX}。请先运行 \`npm run build\`。`)
    process.exit(1)
  }

  let preview = null
  let browser = null
  try {
    preview = await startPreview()
    await waitReady(Date.now() + 15000)
    // 默认用 playwright 内置 chromium（CI 经 `npx playwright install chromium` 装好）。
    // 本地可用 `SMOKE_CHROMIUM_EXECUTABLE` 指向系统 Chrome/Edge 跳过下载。
    const executablePath = process.env.SMOKE_CHROMIUM_EXECUTABLE || undefined
    browser = await chromium.launch({ headless: true, executablePath })
    await runAuthed(browser)
    await runGuest(browser)
  } finally {
    if (browser) await browser.close().catch(() => {})
    if (preview) preview.kill()
  }

  const failed = results.filter((r) => !r.pass)
  console.log(JSON.stringify(results, null, 2))
  process.exit(failed.length ? 1 : 0)
}

main().catch((err) => {
  console.error('smoke 运行失败：', err)
  process.exit(1)
})
