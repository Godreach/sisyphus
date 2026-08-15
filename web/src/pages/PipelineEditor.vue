<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15). Host page for the three editor
// variants, switchable via ?variant=A|B|C (floating switcher bottom-centre).
// "Three variants of the pipeline editor, switchable via ?variant=."
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import VariantA from '../components/editor/VariantA.vue'
import VariantB from '../components/editor/VariantB.vue'
import VariantC from '../components/editor/VariantC.vue'

const { t } = useI18n()
const route = useRoute()
// hash-history: query lives inside the hash (after '?'), route.query covers both
const variant = computed(() => {
  const q = (route.query.variant as string) ?? new URLSearchParams(window.location.hash.split('?')[1] ?? '').get('variant')
  return q ?? 'A'
})
</script>

<template>
  <h1>{{ t('pipeline.editor') }} · main-ci</h1>
  <VariantA v-if="variant === 'A'" />
  <VariantB v-else-if="variant === 'B'" />
  <VariantC v-else />
</template>
