#!/usr/bin/env node
// 全页面截图验收（票 #113 AC2/AC4）：vite demo（MSW 全量挂载 + 真实规模 fixture，
// 无需本机后端）驱动的真实浏览器全页面截图，归档对照定稿设计语言。
//
// 这正是 spec #100 验收关键词「mock 环境当 demo 演示看不出是假的」的证据面：
// 截图用 mock 数据但规模/状态/动态接近真实（11 项目 / 24 流水线 / 200+ 构建 /
// 7 构建机多状态 / 动态构建 SSE）——judge 对照定稿设计语言验收。形态对齐 #103
// 三主页面挑刺的验收口径（`npm run demo`、admin 登录、真实规模 fixture）。
//
// 视口/主题：1440×900 桌面 + 768×900 平板，浅色/深色双主题（深色由
// sisyphus-theme=dark 钉死，不靠系统）。全 16 页（含认证面 login/setup/404）。
// 桌面浅色 + 桌面深色 + 平板浅色 = 3 组 × 16 页 = 48 张。
//
// 用法：`node scripts/screenshots.mjs`（自起 demo dev 服务器，跑完自关）。
// 产物落 `docs/screenshots/web-v1/<combo>/<page>.png`。CI 不跑（验收证据面，
// 非回归门——与 smoke 同形态但出图归档，非断言门）。
//
// 与 smoke.mjs 的分工：smoke 跑 `vite preview`（构建产物）+ page.route 内联
// mock，断言 15 条主路径 DOM 渲染（回归门）；本脚本跑 `vite --mode demo`
// （dev server + MSW 真实规模 fixture）出全页面截图（验收证据）——前者守
// 构建面/路由面，后者守设计语言面/演示真实度面。

import { mkdirSync, rmSync } from 'node:fs'
import { spawn } from 'node:child_process'
import { createRequire } from 'node:module'
import http from 'node:http'
import { setTimeout as sleep } from 'node:timers/promises'

import { chromium } from 'playwright'

const require = createRequire(import.meta.url)
const path = require('path')
const ROOT = path.resolve(import.meta.dirname, '..') // sisyphus-web
const OUT_ROOT = path.resolve(ROOT, '..', 'docs', 'screenshots', 'web-v1')
const PORT = 5180
const BASE = `http://localhost:${PORT}`
// 数据 + SSE 日志回放 + 动画/进度条稳定窗口（mock 延迟 <300ms，SSE 回放 ~1s）。
const SETTLE_MS = 2500

const results = []
const ok = (name, pass, extra = '') => results.push({ name, pass, extra })

// 13 条受保护页（build-detail 用动态 succeeded 号，单独处理）。
// demo fixture：web-app 项目有 main/release/nightly 三条流水线 + 成员 + 机密；
// release 有参数 + 产物上传（编辑器/构建详情渲染面最富）。等待选择器统一用
// 顶栏标题（壳挂载 + 路由解析），SETTLE_MS 兜数据/SSE 落定。
const AUTHED_PAGES = [
  { path: '/', name: 'overview' },
  { path: '/pipelines', name: 'pipelines' },
  { path: '/projects', name: 'projects' },
  { path: '/projects/web-app', name: 'project-detail' },
  { path: '/projects/web-app/pipelines/release', name: 'pipeline-edit' },
  { path: '/projects/web-app/pipelines/release/builds', name: 'build-list' },
  { path: '/machines', name: 'machines' },
  { path: '/agents/build-01', name: 'agent-detail' },
  { path: '/admin/secrets', name: 'admin-secrets' },
  { path: '/admin/audit', name: 'admin-audit' },
  { path: '/admin/upgrade', name: 'admin-upgrade' },
  { path: '/admin/users', name: 'admin-users' },
]

// 认证面（公开页，无壳居中）：登录表单 / 初始化引导 / 404。
const GUEST_PAGES = [
  { path: '/login', name: 'login', wait: 'input[name="username"]' },
  { path: '/setup', name: 'setup', wait: '.setup-intro' },
  { path: '/no-such-page', name: 'not-found', wait: '.not-found-page' },
]

// 三组：桌面浅色 / 桌面深色 / 平板浅色（深色+平板组合 v1 不收）。
const COMBOS = [
  { name: 'desktop-light', theme: 'light', width: 1440, height: 900 },
  { name: 'desktop-dark', theme: 'dark', width: 1440, height: 900 },
  { name: 'tablet-light', theme: 'light', width: 768, height: 900 },
]

// --- demo dev 服务器生命周期 --------------------------------------------------

function startDemoServer() {
  return new Promise((resolve, reject) => {
    // shell:true + 单字符串命令——跨平台经 shell 解析 `npx`，规避「shell:true
    // 配 args 数组」的参数不转义告警（DEP0190），同 smoke.mjs 形态。
    const child = spawn(`npx vite --mode demo --port ${PORT} --strictPort`, {
      cwd: ROOT,
      shell: true,
      stdio: 'ignore',
    })
    child.on('error', reject)
    child.on('exit', (code) => reject(new Error(`vite demo 异常退出（code=${code}）`)))
    resolve(child)
  })
}

async function waitReady(deadline) {
  for (;;) {
    if (Date.now() > deadline) throw new Error(`vite demo 未在 ${(deadline - Date.now()) / 1000 | 0}s 内就绪`)
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

// --- 截图 ---------------------------------------------------------------------

/** 登录 admin（demo mock：任意非空账号密码即可，admin 为管理员）。登录成功
 *  回跳 overview，等顶栏「工作台」确认会话立住（MSW 会话 cookie 生效）。 */
async function loginAsAdmin(page) {
  await page.goto(`${BASE}/login`, { waitUntil: 'domcontentloaded' })
  await page.locator('input[name="username"]').waitFor({ timeout: 15000 })
  await page.locator('input[name="username"]').fill('admin')
  await page.locator('input[name="password"]').fill('admin123')
  await page.locator('button[type="submit"]').click()
  await page.locator('.app-topbar-title', { hasText: '工作台' }).first().waitFor({ timeout: 20000 })
}

/** 解析一条 succeeded release 构建号（build-detail 截图用——succeeded 终态
 *  展示全阶段任务 + 产物 + 完整日志，渲染面最富）。fetch 经 MSW 拦截（会话
 *  cookie 已立），builds handler 支持 ?status=succeeded 过滤（items 按号倒序，
 *  返回最高号的 succeeded 构建——确定性即 web-app/release #4）。失败回落 4
 *  （web-app/release #4：确定性 succeeded——db.ts seed 2290787709 推导，
 *  与 fetch 主路径返回同号，故回落与主路径产物一致）。 */
async function resolveSucceededBuild(page, comboName) {
  try {
    const num = await page.evaluate(async () => {
      const r = await fetch(
        '/api/v1/projects/web-app/pipelines/release/builds?status=succeeded&limit=20',
        { credentials: 'include' },
      )
      if (!r.ok) return null
      const data = await r.json()
      return data?.items?.[0]?.number ?? null
    })
    if (num != null) return num
  } catch (err) {
    ok(`${comboName}/resolve-succeeded-build`, false, err.message)
  }
  return 4 // 回退：web-app/release #4（确定性 succeeded，与 fetch 主路径同号）
}

async function shoot(page, outDir, name, waitSel) {
  try {
    await page.locator(waitSel).first().waitFor({ timeout: 15000 })
    await page.waitForTimeout(SETTLE_MS)
    await page.screenshot({ path: path.join(outDir, `${name}.png`), fullPage: true })
    ok(name, true)
  } catch (err) {
    ok(name, false, err.message)
  }
}

/** 建截图上下文（runAuthed/runGuest 共用）：钉 zh-CN locale + 主题偏好 +
 *  pageerror 收集。主题经 sisyphus-theme=dark 钉死（不靠 prefers-color-scheme），
 *  浅色显式 light（覆盖系统深色设备）。 */
async function createShotContext(browser, combo) {
  const context = await browser.newContext({
    viewport: { width: combo.width, height: combo.height },
    deviceScaleFactor: 1,
    locale: 'zh-CN',
  })
  await context.addInitScript((theme) => {
    window.localStorage.setItem('sisyphus.locale', 'zh-CN')
    window.localStorage.setItem('sisyphus-theme', theme)
  }, combo.theme)
  const page = await context.newPage()
  const errors = []
  page.on('pageerror', (e) => errors.push(String(e)))
  return { context, page, errors }
}

async function runAuthed(browser, combo) {
  const outDir = path.join(OUT_ROOT, combo.name)
  const { context, page, errors } = await createShotContext(browser, combo)

  try {
    await loginAsAdmin(page)
    ok(`${combo.name}/login`, true)
  } catch (err) {
    ok(`${combo.name}/login`, false, err.message)
    await context.close()
    return
  }

  const succeeded = await resolveSucceededBuild(page, combo.name)
  const buildDetailPath = `/projects/web-app/pipelines/release/builds/${succeeded}`

  for (const p of AUTHED_PAGES) {
    await page.goto(`${BASE}${p.path}`, { waitUntil: 'domcontentloaded' })
    // 与 shoot 分开：shoot 的 waitSel 在 goto 之后才等——这里先等顶栏确认壳挂载，
    // 再交 SETTLE_MS 兜数据落定。顶栏标题是路由 meta（即时渲染），数据靠 SETTLE。
    await shoot(page, outDir, p.name, '.app-topbar-title')
  }
  // build-detail（动态 succeeded 号）。
  await page.goto(`${BASE}${buildDetailPath}`, { waitUntil: 'domcontentloaded' })
  await shoot(page, outDir, 'build-detail', '.app-topbar-title')

  for (const e of errors) ok(`${combo.name}/pageerror: ${e.slice(0, 120)}`, false, e)
  await context.close()
}

async function runGuest(browser, combo) {
  const outDir = path.join(OUT_ROOT, combo.name)
  const { context, page, errors } = await createShotContext(browser, combo)
  for (const p of GUEST_PAGES) {
    await page.goto(`${BASE}${p.path}`, { waitUntil: 'domcontentloaded' })
    await shoot(page, outDir, p.name, p.wait)
  }
  for (const e of errors) ok(`${combo.name}/guest pageerror: ${e.slice(0, 120)}`, false, e)
  await context.close()
}

async function main() {
  rmSync(OUT_ROOT, { recursive: true, force: true })
  mkdirSync(OUT_ROOT, { recursive: true })

  let server = null
  let browser = null
  try {
    server = await startDemoServer()
    await waitReady(Date.now() + 30000)
    const executablePath = process.env.SMOKE_CHROMIUM_EXECUTABLE || undefined
    browser = await chromium.launch({ headless: true, executablePath })
    for (const combo of COMBOS) {
      mkdirSync(path.join(OUT_ROOT, combo.name), { recursive: true })
      await runAuthed(browser, combo)
      await runGuest(browser, combo)
    }
  } finally {
    if (browser) await browser.close().catch(() => {})
    if (server) server.kill()
  }

  const failed = results.filter((r) => !r.pass)
  console.log(JSON.stringify(results, null, 2))
  process.exit(failed.length ? 1 : 0)
}

main().catch((err) => {
  console.error('screenshots 运行失败：', err)
  process.exit(1)
})
