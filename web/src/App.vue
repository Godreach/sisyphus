<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import PrototypeSwitcher from './components/PrototypeSwitcher.vue'

const { t, locale } = useI18n()
const route = useRoute()
const showChrome = computed(() => route.path !== '/setup')
const isEditor = computed(() => route.path.startsWith('/pipelines/'))

function toggleLocale() {
  locale.value = locale.value === 'zh' ? 'en' : 'zh'
  localStorage.setItem('proto-locale', locale.value)
}
</script>

<template>
  <div class="app" :class="{ bare: !showChrome }">
    <aside v-if="showChrome" class="sidebar">
      <div class="logo">⬢ sisyphus</div>
      <nav>
        <RouterLink to="/overview">{{ t('nav.overview') }}</RouterLink>
        <RouterLink to="/projects">{{ t('nav.projects') }}</RouterLink>
        <RouterLink to="/agents">{{ t('nav.agents') }}</RouterLink>
        <div class="nav-group">{{ t('nav.admin') }}</div>
        <RouterLink to="/admin/secrets" class="sub">{{ t('nav.secrets') }}</RouterLink>
        <RouterLink to="/admin/audit" class="sub">{{ t('nav.audit') }}</RouterLink>
        <RouterLink to="/admin/upgrade" class="sub">{{ t('nav.upgrade') }}</RouterLink>
        <RouterLink to="/admin/users" class="sub">{{ t('nav.users') }}</RouterLink>
      </nav>
      <div class="sidebar-footer">
        <button class="lang" @click="toggleLocale">{{ t('nav.langToggle') }}</button>
        <span class="who">tanweijian · admin</span>
      </div>
    </aside>

    <main v-if="showChrome" class="content">
      <div class="proto-banner">{{ t('common.prototypeNote') }}</div>
      <RouterView />
    </main>
    <main v-else class="content bare">
      <RouterView />
    </main>

    <PrototypeSwitcher v-if="isEditor" />
  </div>
</template>

<style>
:root {
  --bg: #f5f6f8;
  --panel: #ffffff;
  --ink: #1f2430;
  --ink-dim: #6b7280;
  --line: #e3e6eb;
  --accent: #2563eb;
  --ok: #16a34a;
  --warn: #d97706;
  --err: #dc2626;
  --unknown: #7c3aed;
  --mono: ui-monospace, 'Cascadia Code', Consolas, monospace;
}
* { box-sizing: border-box; }
body { margin: 0; font-family: 'Segoe UI', 'PingFang SC', 'Microsoft YaHei', system-ui, sans-serif; background: var(--bg); color: var(--ink); font-size: 14px; }
a { color: inherit; text-decoration: none; }
button { font: inherit; cursor: pointer; }
.app { display: grid; grid-template-columns: 216px 1fr; min-height: 100vh; }
.app.bare { grid-template-columns: 1fr; }

.sidebar { background: #101623; color: #cbd5e1; display: flex; flex-direction: column; padding: 16px 12px; }
.logo { font-weight: 700; font-size: 17px; color: #fff; padding: 4px 10px 16px; }
.sidebar nav { display: flex; flex-direction: column; gap: 2px; flex: 1; }
.sidebar nav a { padding: 7px 10px; border-radius: 6px; color: #94a3b8; }
.sidebar nav a:hover { background: #1c2436; color: #e2e8f0; }
.sidebar nav a.router-link-active { background: #1d4ed8; color: #fff; }
.nav-group { padding: 14px 10px 4px; font-size: 11px; text-transform: uppercase; letter-spacing: .08em; color: #475569; }
.sidebar nav a.sub { padding-left: 22px; }
.sidebar-footer { border-top: 1px solid #1c2436; padding-top: 10px; display: flex; align-items: center; gap: 10px; }
.lang { background: #1c2436; color: #cbd5e1; border: 1px solid #2b3752; border-radius: 6px; padding: 4px 10px; }
.who { font-size: 12px; color: #64748b; }

.content { padding: 20px 28px 60px; }
.proto-banner { background: #fef3c7; border: 1px solid #fcd34d; color: #92400e; padding: 5px 12px; border-radius: 6px; font-size: 12px; margin-bottom: 16px; }

h1 { font-size: 20px; margin: 0 0 16px; }
h2 { font-size: 15px; margin: 20px 0 8px; }
.card { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 14px 16px; }
.row { display: flex; gap: 10px; align-items: center; }
table.tbl { width: 100%; border-collapse: collapse; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
table.tbl th { text-align: left; font-size: 12px; color: var(--ink-dim); padding: 8px 12px; border-bottom: 1px solid var(--line); background: #fafbfc; }
table.tbl td { padding: 8px 12px; border-bottom: 1px solid var(--line); }
table.tbl tr:last-child td { border-bottom: none; }

.badge { display: inline-flex; align-items: center; gap: 5px; font-size: 12px; border-radius: 999px; padding: 2px 9px; font-weight: 600; }
.badge::before { content: ''; width: 7px; height: 7px; border-radius: 50%; background: currentColor; }
.b-ok { color: var(--ok); background: #dcfce7; }
.b-err { color: var(--err); background: #fee2e2; }
.b-run { color: var(--accent); background: #dbeafe; }
.b-warn { color: var(--warn); background: #ffedd5; }
.b-unknown { color: var(--unknown); background: #ede9fe; }
.b-dim { color: var(--ink-dim); background: #eef0f3; }

.btn { border: 1px solid var(--line); background: #fff; border-radius: 6px; padding: 5px 12px; }
.btn:hover { border-color: #c7ccd4; }
.btn.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
.btn.danger { color: var(--err); }
input[type=text], select { font: inherit; padding: 5px 8px; border: 1px solid var(--line); border-radius: 6px; background: #fff; }
code, .mono { font-family: var(--mono); font-size: 12.5px; }
</style>
