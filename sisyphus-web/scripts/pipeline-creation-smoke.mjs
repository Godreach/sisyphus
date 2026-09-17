#!/usr/bin/env node
// #116: real browser / demo MSW acceptance. Start `vite --mode demo` first.
// All screenshots and browser work directories stay under repository target/.
import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import path from 'node:path'
import { chromium } from 'playwright'

const base = process.env.PIPELINE_SMOKE_BASE ?? 'http://localhost:5173'
const output = path.resolve(import.meta.dirname, '../../target/issue-116/browser')
mkdirSync(output, { recursive: true })
const browser = await chromium.launch({ headless: true, executablePath: process.env.SMOKE_CHROMIUM_EXECUTABLE || undefined })
try {
  for (const width of [1440, 768]) {
    for (const locale of ['zh-CN', 'en-US']) {
      for (const theme of ['light', 'dark']) {
        const label = `${width}-${locale}-${theme}`
        const context = await browser.newContext({ viewport: { width, height: 1000 }, locale })
        try {
          await context.addInitScript(({ locale, theme }) => {
            localStorage.setItem('sisyphus.locale', locale)
            localStorage.setItem('sisyphus-theme', theme)
          }, { locale, theme })
          const page = await context.newPage()
          const errors = []
          const writes = []
          page.on('pageerror', error => errors.push(String(error)))
          page.on('request', request => {
            if (request.method() === 'PUT' && /\/pipelines\/[^/]+$/.test(new URL(request.url()).pathname)) writes.push(request)
          })
          await page.goto(`${base}/login`)
          await page.locator('input[name="username"]').fill('admin')
          await page.locator('input[name="password"]').fill('admin123')
          await page.locator('button[type="submit"]').click()
          await page.waitForURL(`${base}/`)
          await page.goto(`${base}/pipelines?group=flat`)
          const cta = page.getByTestId('topbar-cta')
          await cta.click()
          const dialog = page.getByTestId('new-pipeline-dialog')
          const name = page.locator('input[name="new-pipeline-name"]')
          await name.waitFor()
          assert.equal(await page.getByTestId('new-pipeline-create').isDisabled(), true)
          await name.fill('cancel-this')
          // Naive UI must trap keyboard focus even at the button boundaries.
          for (let i = 0; i < 12; i++) {
            await page.keyboard.press('Tab')
            assert.equal(await page.evaluate(() => document.activeElement?.closest('.n-modal') != null), true, `${label}: focus escaped modal`)
          }
          await page.keyboard.press('Escape')
          await page.waitForURL(`${base}/pipelines?group=flat`)
          await page.waitForFunction(() => document.activeElement?.getAttribute('data-testid') === 'topbar-cta')
          await cta.click()
          await name.waitFor()
          assert.equal(await name.inputValue(), '')
          await page.getByTestId('new-pipeline-project').click()
          await page.keyboard.type('web-app')
          await page.keyboard.press('ArrowDown')
          await page.keyboard.press('Enter')
          await name.fill(`浏览器-${label}`)
          await page.locator('.n-base-select-menu').waitFor({ state: 'hidden' })
          const box = await dialog.boundingBox()
          assert.ok(box && box.x >= 0 && box.x + box.width <= width, `${label}: dialog overflow`)
          assert.equal(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth), true)
          await page.screenshot({ path: path.join(output, `${label}-dialog.png`) })
          await name.press('Enter')
          await page.getByTestId('editor-new-badge').waitFor()
          assert.equal(writes.length, 0, `${label}: precreation write`)
          await page.locator('[name="track-add-stage"]').click()
          await page.getByTestId('editor-back').click()
          await page.getByTestId('unsaved-continue').click()
          await page.getByTestId('unsaved-continue').waitFor({ state: 'hidden' })
          await page.screenshot({ path: path.join(output, `${label}-draft.png`) })
          await page.locator('[name="editor-save"]').click()
          await page.waitForFunction(() => document.querySelector('.editor-rev-value')?.textContent?.trim() === '1')
          assert.equal(writes[0].headers()['if-none-match'], '*')
          assert.equal(new URL(page.url()).searchParams.has('create'), false)
          await page.locator('[name="editor-save"]').click()
          await page.waitForFunction(() => document.querySelector('.editor-rev-value')?.textContent?.trim() === '2')
          assert.equal(writes[1].headers()['if-none-match'], undefined)
          await page.goBack()
          await page.waitForURL(`${base}/pipelines?group=flat`)
          assert.deepEqual(errors, [])
          console.log(`PASS ${label}: focus / Escape / Enter / draft / conditional save / return`)
        } finally {
          await context.close()
        }
      }
    }
  }
} finally {
  await browser.close()
}
