<script setup lang="ts">
import { RefreshCw } from "@lucide/vue"
import { computed } from "vue"
import { Button } from "@/components/ui/button"

defineOptions({
  inheritAttrs: false,
})

const props = withDefaults(
  defineProps<{
    /** loading 为 true 时图标 spin；若未显式传 disabled，则 disabled 默认随 loading */
    loading?: boolean
    /** 显式覆盖 disabled；不传则默认等于 loading */
    disabled?: boolean
    /** 按钮尺寸，对应 shadcn Button 的 size 变体 */
    size?: "default" | "sm"
  }>(),
  {
    loading: false,
    size: "sm",
  },
)

const emit = defineEmits<{
  (e: "click"): void
}>()

// disabled 未显式传入时，默认随 loading
// 注意：需要区分"未传"与"传了 false"，用 undefined 哨兵
const isDisabled = computed(() => props.disabled ?? props.loading)

function onClick() {
  emit("click")
}
</script>

<template>
  <Button
    :variant="'outline'"
    :size="size"
    :disabled="isDisabled"
    :class="$attrs.class"
    @click="onClick"
  >
    <RefreshCw :class="loading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
    {{ $t("common.refresh") }}
  </Button>
</template>
