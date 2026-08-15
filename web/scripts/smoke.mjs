// PROTOTYPE - throwaway (ticket #15). Headless smoke test of the production
// build: clicks every interactive surface the IAB couldn't reliably click.
import { chromium } from 'playwright-core'

const base = 'http://localhost:5299'
const exe = 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe'
const results = []
const ok = (name, pass, extra = '') => results.push({ name, pass, extra })

const browser = await chromium.launch({ executablePath: exe, headless: true })
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
page.on('pageerror', (e) => results.push({ name: 'PAGE ERROR', pass: false, extra: String(e) }))

// 1. wizard next/skip
await page.goto(`${base}/#/setup`)
await page.waitForTimeout(800)
await page.getByRole('button', { name: '下一步' }).click()
await page.waitForTimeout(300)
ok('wizard next -> step2', (await page.getByRole('heading', { name: '注册第一个 Agent' }).count()) === 1)
await page.getByRole('button', { name: '跳过此步' }).click()
await page.waitForTimeout(300)
ok('wizard skip -> step3', (await page.getByRole('heading', { name: '创建第一个项目' }).count()) === 1)

// 2. language toggle
await page.goto(`${base}/#/overview`)
await page.waitForTimeout(600)
await page.getByRole('button', { name: 'EN' }).click()
await page.waitForTimeout(300)
ok('i18n zh->en', (await page.getByRole('link', { name: 'Projects' }).count()) === 1)
await page.getByRole('button', { name: '中文' }).click()
await page.waitForTimeout(300)
ok('i18n en->zh', (await page.getByRole('link', { name: '项目' }).count()) === 1)

// 3. editor variant B: tabs + JSON toggle + outline selection
await page.goto(`${base}/#/pipelines/pl1/edit?variant=B`)
await page.waitForTimeout(800)
await page.getByRole('button', { name: 'JSON +' }).click()
await page.waitForTimeout(300)
ok('variant B JSON toggle', (await page.locator('pre.json').count()) === 1)
await page.locator('div.outline-job', { hasText: 'build-windows' }).click()
await page.waitForTimeout(300)
ok('variant B outline select', (await page.getByRole('heading', { name: '构建 / build-windows' }).count()) === 1)

// 4. switcher arrows cycle variants
await page.getByRole('button', { name: 'next variant' }).click()
await page.waitForTimeout(500)
ok('switcher B->C', page.url().includes('variant=C'), page.url())
ok('variant C rendered', (await page.locator('.variant-c').count()) === 1)
await page.keyboard.press('ArrowLeft')
await page.waitForTimeout(500)
ok('keyboard back to B', page.url().includes('variant=B'), page.url())

// 5. variant A canvas
await page.goto(`${base}/#/pipelines/pl1/edit?variant=A`)
await page.waitForTimeout(900)
ok('variant A canvas nodes', (await page.locator('.vue-flow__node').count()) >= 5)

// 6. agents + admin pages render
await page.goto(`${base}/#/agents`)
await page.waitForTimeout(500)
ok('agents table', (await page.getByText('build-linux-01').count()) >= 1)
await page.goto(`${base}/#/admin/secrets`)
await page.waitForTimeout(500)
ok('secrets page', (await page.getByText('NPM_TOKEN').count()) === 1)
await page.goto(`${base}/#/admin/upgrade`)
await page.waitForTimeout(500)
ok('upgrade page', (await page.getByText('1.0.3').count()) >= 1)

await browser.close()
const failed = results.filter((r) => !r.pass)
console.log(JSON.stringify(results, null, 2))
process.exit(failed.length ? 1 : 0)
