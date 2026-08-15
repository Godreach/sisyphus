import { createApp } from 'vue'
import { createRouter, createWebHashHistory } from 'vue-router'
import { createI18n } from 'vue-i18n'
import App from './App.vue'
import { zh } from './i18n/zh'
import { en } from './i18n/en'

// PROTOTYPE - throwaway (ticket #15). Not production code.
const i18n = createI18n({
  legacy: false,
  locale: localStorage.getItem('proto-locale') ?? 'zh',
  fallbackLocale: 'zh',
  messages: { zh, en },
})

const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    { path: '/', redirect: '/overview' },
    { path: '/overview', component: () => import('./pages/Overview.vue') },
    { path: '/projects', component: () => import('./pages/Projects.vue') },
    { path: '/projects/:id', component: () => import('./pages/ProjectDetail.vue') },
    { path: '/pipelines/:id/edit', component: () => import('./pages/PipelineEditor.vue') },
    { path: '/builds/:id', component: () => import('./pages/BuildDetail.vue') },
    { path: '/agents', component: () => import('./pages/Agents.vue') },
    { path: '/agents/:id', component: () => import('./pages/AgentDetail.vue') },
    { path: '/admin/secrets', component: () => import('./pages/AdminSecrets.vue') },
    { path: '/admin/audit', component: () => import('./pages/AdminAudit.vue') },
    { path: '/admin/upgrade', component: () => import('./pages/AdminUpgrade.vue') },
    { path: '/admin/users', component: () => import('./pages/AdminUsers.vue') },
    { path: '/setup', component: () => import('./pages/SetupWizard.vue') },
  ],
})

const app = createApp(App)
app.use(i18n)
app.use(router)
app.mount('#app')
