<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15). Floating variant switcher per /prototype UI.md:
// left/right arrows cycle variants, updates ?variant= so it's shareable/reload-stable.
// Clearly not part of the design being evaluated.
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

const props = defineProps<{ variants?: { key: string; label: string }[] }>()

const defaults = [
  { key: 'A', label: 'pipeline.variantA' },
  { key: 'B', label: 'pipeline.variantB' },
  { key: 'C', label: 'pipeline.variantC' },
]
const list = computed(() => props.variants ?? defaults)
const route = useRoute()
const router = useRouter()
const { t } = useI18n()

const current = computed(() => new URLSearchParams(window.location.hash.split('?')[1] ?? '').get('variant') ?? (route.query.variant as string) ?? 'A')

function cycle(dir: 1 | -1) {
  const idx = list.value.findIndex((v) => v.key === current.value)
  const next = list.value[(idx + dir + list.value.length) % list.value.length]
  const [path] = window.location.hash.split('?')
  window.location.hash = `${path}?variant=${next.key}`
}

function onKey(e: KeyboardEvent) {
  const el = e.target as HTMLElement
  if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable) return
  if (e.key === 'ArrowLeft') cycle(-1)
  if (e.key === 'ArrowRight') cycle(1)
}
</script>

<template>
  <div class="switcher" tabindex="0" @keydown="onKey">
    <button @click="cycle(-1)" aria-label="prev variant">←</button>
    <span class="label">{{ current }} · {{ t(list.find((v) => v.key === current)?.label ?? '') }}</span>
    <button @click="cycle(1)" aria-label="next variant">→</button>
  </div>
</template>

<style scoped>
.switcher {
  position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%);
  display: flex; align-items: center; gap: 10px;
  background: #111827; color: #f9fafb; padding: 8px 14px; border-radius: 999px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.35); z-index: 999;
  outline: none;
}
.switcher button { background: #1f2937; color: #f9fafb; border: 1px solid #374151; border-radius: 999px; width: 28px; height: 28px; line-height: 1; }
.switcher button:hover { background: #374151; }
.label { font-size: 13px; font-weight: 600; white-space: nowrap; }
</style>
