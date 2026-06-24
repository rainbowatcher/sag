<script setup lang="ts">
import { computed, ref, onMounted, onUnmounted } from 'vue'

const props = defineProps<{ svg: string }>()

function decodeBase64(str: string): string {
  if (typeof Buffer !== 'undefined') {
    return Buffer.from(str, 'base64').toString('utf-8')
  }
  const binary = atob(str)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i)
  }
  return new TextDecoder().decode(bytes)
}

const decoded = computed(() => {
  try {
    return decodeBase64(props.svg)
  } catch {
    return props.svg
  }
})

const maximized = ref(false)

function onKeyDown(e: KeyboardEvent) {
  if (e.key === 'Escape') maximized.value = false
}

onMounted(() => document.addEventListener('keydown', onKeyDown))
onUnmounted(() => document.removeEventListener('keydown', onKeyDown))
</script>

<template>
  <div class="mermaid-container">
    <div class="mermaid" v-html="decoded" />
    <button class="mermaid-maximize-btn" title="放大查看" @click="maximized = true">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <polyline points="15 3 21 3 21 9" />
        <polyline points="9 21 3 21 3 15" />
        <line x1="21" y1="3" x2="14" y2="10" />
        <line x1="3" y1="21" x2="10" y2="14" />
      </svg>
    </button>

    <Teleport to="body">
      <div v-if="maximized" class="mermaid-overlay" @click.self="maximized = false">
        <div class="mermaid-overlay-content">
          <div class="mermaid" v-html="decoded" />
        </div>
        <button class="mermaid-close-btn" title="关闭" @click="maximized = false">
          <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <line x1="18" y1="6" x2="6" y2="18" />
            <line x1="6" y1="6" x2="18" y2="18" />
          </svg>
        </button>
      </div>
    </Teleport>
  </div>
</template>
