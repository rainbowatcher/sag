<script setup lang="ts">
import { ArrowDown, ArrowUp, EllipsisVertical, Plus, Trash2 } from "@lucide/vue"
import Combobox from "@/components/Combobox.vue"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Textarea } from "@/components/ui/textarea"
import { useRouteStore } from "@/stores/routes"
import type { Provider, TargetItem } from "@/types/api"

const routeStore = useRouteStore()

const props = defineProps<{
  modelValue: TargetItem[]
  providers: Provider[]
  providerModels: Record<string, string[]>
  providerModelsLoading: boolean
  showPriorityControls?: boolean
}>()

const emit = defineEmits<{
  "update:modelValue": [TargetItem[]]
}>()

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
    { provider: "", real_model: "", priority: 0, retries: 0, extra: "" },
  ])
}

function removeTarget(index: number) {
  if (props.modelValue.length > 1) {
    updateTargets([...props.modelValue.slice(0, index), ...props.modelValue.slice(index + 1)])
  }
}

function swapPriority(i: number, j: number) {
  if (j < 0 || j >= props.modelValue.length) return
  const newTargets = [...props.modelValue]
  const tmp = newTargets[i].priority
  newTargets[i].priority = newTargets[j].priority
  newTargets[j].priority = tmp
  updateTargets(newTargets)
}

function onProviderChange(index: number, value: string) {
  updateTarget(index, { provider: value, real_model: "" })
  routeStore.loadProviderModels(value, props.providers)
}
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
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead class="w-8 text-center">#</TableHead>
            <TableHead>Provider</TableHead>
            <TableHead>Model</TableHead>
            <TableHead class="text-right">Actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          <TableRow v-for="(t, i) in modelValue" :key="`${i}-${t.provider}-${t.real_model}`">
            <TableCell class="text-muted-foreground text-center text-xs font-medium">
              {{ i + 1 }}
            </TableCell>
            <TableCell>
              <Combobox
                :model-value="t.provider"
                :options="
                  providers.map((p) => ({ value: p.name, label: `${p.name} (${p.provider_type})` }))
                "
                placeholder="Select provider..."
                search-placeholder="Search provider..."
                empty-text="No providers found"
                @update:model-value="(v) => onProviderChange(i, v as string)"
              />
            </TableCell>
            <TableCell>
              <Combobox
                :model-value="t.real_model"
                :options="(providerModels[t.provider] ?? []).map((m) => ({ value: m }))"
                placeholder="Select model"
                search-placeholder="Search model..."
                empty-text="No models found"
                :loading="providerModelsLoading && !providerModels[t.provider]"
                @update:model-value="(v) => updateTarget(i, { real_model: v as string })"
              />
            </TableCell>
            <TableCell class="text-right">
              <div class="flex items-center justify-end gap-0.5">
                <Popover>
                  <PopoverTrigger as-child>
                    <Button variant="ghost" size="icon" class="size-8">
                      <EllipsisVertical class="h-3.5 w-3.5" />
                    </Button>
                  </PopoverTrigger>
                  <PopoverContent side="bottom" align="end" class="w-80">
                    <div class="space-y-3">
                      <h4 class="text-sm font-medium">Target Configuration</h4>
                      <div>
                        <Label class="text-xs">Priority</Label>
                        <Input
                          type="number"
                          :model-value="t.priority"
                          @input="
                            updateTarget(i, {
                              priority: Number(($event.target as HTMLInputElement).value),
                            })
                          "
                        />
                      </div>
                      <div>
                        <Label class="text-xs">Retries</Label>
                        <Input
                          type="number"
                          :model-value="t.retries"
                          @input="
                            updateTarget(i, {
                              retries: Number(($event.target as HTMLInputElement).value),
                            })
                          "
                        />
                      </div>
                      <div>
                        <Label class="text-xs">Extra (JSON)</Label>
                        <Textarea
                          :model-value="t.extra"
                          @update:model-value="updateTarget(i, { extra: String($event) })"
                          placeholder='{"model_id": 39}'
                          rows="4"
                        />
                      </div>
                    </div>
                  </PopoverContent>
                </Popover>

                <template v-if="showPriorityControls">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="size-8"
                    :disabled="i === 0"
                    @click="swapPriority(i, i - 1)"
                  >
                    <ArrowUp class="h-3.5 w-3.5" />
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="size-8"
                    :disabled="i === modelValue.length - 1"
                    @click="swapPriority(i, i + 1)"
                  >
                    <ArrowDown class="h-3.5 w-3.5" />
                  </Button>
                </template>

                <Button
                  variant="ghost"
                  size="icon"
                  class="text-destructive hover:text-destructive size-8"
                  :disabled="modelValue.length === 1"
                  @click="removeTarget(i)"
                >
                  <Trash2 class="h-4 w-4" />
                </Button>
              </div>
            </TableCell>
          </TableRow>
        </TableBody>
      </Table>
    </div>
  </div>
</template>
