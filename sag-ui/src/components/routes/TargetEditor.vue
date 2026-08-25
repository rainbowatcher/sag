<script setup lang="ts">
import { ArrowDown, ArrowUp, EllipsisVertical, Plus, Trash2 } from "@lucide/vue"
import { computed, ref } from "vue"
import Combobox, { type ComboboxOption } from "@/components/Combobox.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Textarea } from "@/components/ui/textarea"
import { validateExtraJson } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { Provider, TargetItem } from "@/types/api"

const routeStore = useRouteStore()

const props = defineProps<{
  modelValue: TargetItem[]
  providers: Provider[]
  providerModels: Record<string, string[]>
  providerModelsLoading: boolean
}>()

const emit = defineEmits<{
  "update:modelValue": [TargetItem[]]
}>()

// --- ID generation ---
const nextId = computed(() => {
  const ids = props.modelValue.map((t) => t.id ?? 0)
  return ids.length > 0 ? Math.max(...ids) + 1 : 1
})

// --- Sort state ---
const sortDirection = ref<"desc" | "asc">("desc")

// --- Expand state: which target's config bar is open (tracked by id, stable across sorts) ---
const expandedId = ref<number | null>(null)
const frozenOrder = ref<TargetItem[] | null>(null)

// --- Display order: frozen while editing, sorted by priority when collapsed ---
const displayTargets = computed(() => {
  if (frozenOrder.value !== null) {
    // 编辑区打开期间：按快照顺序，但字段值取 modelValue 最新数据
    return frozenOrder.value
      .map((snap) => props.modelValue.find((t) => t.id === snap.id) ?? snap)
      .filter(Boolean) as TargetItem[]
  }
  // 编辑区关闭：按 priority 排序
  const targets = [...props.modelValue]
  targets.sort((a, b) => {
    const cmp = (a.priority ?? 0) - (b.priority ?? 0)
    return sortDirection.value === "desc" ? -cmp : cmp
  })
  return targets
})

// --- Priority badge ---
const maxPriority = computed(() => Math.max(1, ...props.modelValue.map((t) => t.priority ?? 0)))

function priorityBadgeVariant(p: number): "default" | "secondary" | "outline" {
  if (p <= 0) return "outline"
  const ratio = p / maxPriority.value
  if (ratio >= 0.8) return "default"
  if (ratio >= 0.4) return "secondary"
  return "outline"
}

function toggleSort() {
  sortDirection.value = sortDirection.value === "desc" ? "asc" : "desc"
}

function toggleExpand(id: number) {
  if (expandedId.value === id) {
    // 关闭：先清快照，computed 重新按 priority 排序
    expandedId.value = null
    frozenOrder.value = null
  } else {
    // 展开：先拍当前顺序快照，再记录展开的 id
    frozenOrder.value = [...displayTargets.value]
    expandedId.value = id
  }
}

// --- Provider & Model Options Helpers ---
function getProviderOptions(target: TargetItem): ComboboxOption[] {
  const list: ComboboxOption[] = props.providers.map((p) => ({
    value: p.name,
    label: `${p.name} (${p.provider_type})${!p.enabled ? " — Disabled" : ""}`,
    warning: !p.enabled,
  }))
  if (target.provider && !props.providers.some((p) => p.name === target.provider)) {
    list.unshift({
      value: target.provider,
      label: `${target.provider} (Not Found)`,
      warning: true,
    })
  }
  return list
}

function getModelOptions(target: TargetItem): ComboboxOption[] {
  const models = props.providerModels[target.provider] ?? []
  const options: ComboboxOption[] = models.map((m) => ({ value: m, label: m }))
  if (target.real_model && !models.includes(target.real_model)) {
    const p = props.providers.find((prov) => prov.name === target.provider)
    const isInvalid =
      p && p.enabled_models.length > 0 && !p.enabled_models.includes(target.real_model)
    options.unshift({
      value: target.real_model,
      label: isInvalid ? `${target.real_model} (Unavailable / Not in provider)` : target.real_model,
      warning: isInvalid,
    })
  }
  return options
}

function isTargetWarning(target: TargetItem): boolean {
  if (!target.provider) return false
  const p = props.providers.find((prov) => prov.name === target.provider)
  if (!p || !p.enabled) return true
  if (
    target.real_model &&
    p.enabled_models.length > 0 &&
    !p.enabled_models.includes(target.real_model)
  ) {
    return true
  }
  return false
}

// --- Mutations ---

function updateTargets(val: TargetItem[]) {
  emit("update:modelValue", val)
}

function updateTarget(index: number, patch: Partial<TargetItem>) {
  const newTargets = props.modelValue.map((t, i) => (i === index ? { ...t, ...patch } : t))
  updateTargets(newTargets)
}

function addTarget() {
  updateTargets([
    ...props.modelValue,
    { id: nextId.value, provider: "", real_model: "", priority: 0, retries: 0, extra: "" },
  ])
}

function removeTarget(index: number) {
  if (props.modelValue.length > 1) {
    const updated = props.modelValue.filter((_, i) => i !== index)
    // If we removed the expanded row, collapse
    if (expandedId.value === props.modelValue[index].id) {
      expandedId.value = null
    }
    updateTargets(updated)
  }
}

function onProviderChange(index: number, value: string) {
  updateTarget(index, { provider: value, real_model: "" })
  routeStore.loadProviderModels(value, props.providers)
}

// --- Extra validation ---
</script>

<template>
  <div class="mt-6 space-y-3">
    <div class="flex items-center justify-between">
      <p class="text-sm font-medium">Targets</p>
      <Button type="button" variant="outline" size="sm" @click="addTarget">
        <Plus class="mr-1 h-3.5 w-3.5" />
        Add Target
      </Button>
    </div>

    <div class="rounded-lg border">
      <Table class="table-fixed">
        <TableHeader>
          <TableRow>
            <TableHead class="w-[5%] text-center">#</TableHead>
            <TableHead class="w-[15%] cursor-pointer select-none" @click="toggleSort">
              <div class="flex items-center gap-1">
                Priority
                <ArrowDown v-if="sortDirection === 'desc'" class="h-3 w-3" />
                <ArrowUp v-else class="h-3 w-3" />
              </div>
            </TableHead>
            <TableHead class="w-[35%]">Provider</TableHead>
            <TableHead class="w-[35%]">Model</TableHead>
            <TableHead class="w-[10%] text-right">Actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          <template v-for="(t, displayIndex) in displayTargets" :key="t.id">
            <TableRow
              :class="{
                'text-muted-foreground bg-amber-500/10 dark:bg-amber-500/15': isTargetWarning(t),
              }"
            >
              <TableCell class="text-muted-foreground text-center text-xs font-medium">
                {{ displayIndex + 1 }}
              </TableCell>
              <TableCell>
                <Badge :variant="priorityBadgeVariant(t.priority)" class="font-mono text-xs">
                  {{ t.priority ?? "—" }}
                </Badge>
              </TableCell>
              <TableCell>
                <Combobox
                  :model-value="t.provider"
                  :options="getProviderOptions(t)"
                  placeholder="Select provider..."
                  search-placeholder="Search provider..."
                  empty-text="No providers found"
                  @update:model-value="(v) => onProviderChange(modelValue.indexOf(t), v as string)"
                />
              </TableCell>
              <TableCell>
                <Combobox
                  :model-value="t.real_model"
                  :options="getModelOptions(t)"
                  placeholder="Select model"
                  search-placeholder="Search model..."
                  empty-text="No models found"
                  :loading="providerModelsLoading && !providerModels[t.provider]"
                  @update:model-value="
                    (v) => updateTarget(modelValue.indexOf(t), { real_model: v as string })
                  "
                />
              </TableCell>
              <TableCell class="text-right">
                <div class="flex items-center justify-end gap-0.5">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="size-8"
                    :aria-label="expandedId === t.id ? 'Close configuration' : 'Open configuration'"
                    @click="toggleExpand(t.id)"
                  >
                    <EllipsisVertical class="h-3.5 w-3.5" />
                  </Button>

                  <Button
                    variant="ghost"
                    size="icon"
                    class="text-destructive hover:text-destructive size-8"
                    :disabled="modelValue.length === 1"
                    aria-label="Remove target"
                    @click="removeTarget(modelValue.indexOf(t))"
                  >
                    <Trash2 class="h-4 w-4" />
                  </Button>
                </div>
              </TableCell>
            </TableRow>

            <!-- Inline config bar -->
            <TableRow v-if="expandedId === t.id">
              <TableCell :colspan="5" class="bg-muted/30">
                <div class="flex items-start gap-3 py-1">
                  <div class="w-24">
                    <Label class="text-xs">Priority</Label>
                    <Input
                      type="number"
                      :model-value="t.priority ?? ''"
                      @update:model-value="
                        (v) => updateTarget(modelValue.indexOf(t), { priority: Number(v) })
                      "
                    />
                  </div>
                  <div class="w-24">
                    <Label class="text-xs">Retries</Label>
                    <Input
                      type="number"
                      :model-value="t.retries"
                      @update:model-value="
                        (v) => updateTarget(modelValue.indexOf(t), { retries: Number(v) })
                      "
                    />
                  </div>
                  <div class="flex-1">
                    <Label class="text-xs">Extra (JSON)</Label>
                    <Textarea
                      :model-value="t.extra"
                      @update:model-value="
                        (v) => updateTarget(modelValue.indexOf(t), { extra: String(v) })
                      "
                      placeholder='{"model_id": 39}'
                      rows="1"
                    />
                    <p
                      v-if="t.extra.trim()"
                      class="text-xs"
                      :class="
                        validateExtraJson(t.extra) ? 'text-destructive' : 'text-muted-foreground'
                      "
                    >
                      {{ validateExtraJson(t.extra) ?? "Valid JSON" }}
                    </p>
                  </div>
                </div>
              </TableCell>
            </TableRow>
          </template>
        </TableBody>
      </Table>
    </div>
  </div>
</template>
