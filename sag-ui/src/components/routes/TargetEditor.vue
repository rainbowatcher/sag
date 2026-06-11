<script setup lang="ts">
import { ArrowDown, ArrowUp, Plus, Settings2, Trash2 } from "@lucide/vue"
import Combobox from "@/components/Combobox.vue"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { Textarea } from "@/components/ui/textarea"
import { useRouteStore } from "@/stores/routes"
import type { Provider, TargetItem } from "@/types/api"

const routeStore = useRouteStore()

const props = defineProps<{
  targets: TargetItem[]
  providers: Provider[]
  providerModels: Record<string, string[]>
  providerModelsLoading: boolean
  showPriorityControls?: boolean
}>()

function addTarget() {
  props.targets.push({
    provider: "",
    real_model: "",
    priority: 0,
    retries: 0,
    extra: "",
  })
}

function removeTarget(index: number) {
  if (props.targets.length > 1) {
    props.targets.splice(index, 1)
  }
}

function swapPriority(i: number, j: number) {
  if (j < 0 || j >= props.targets.length) return
  const tmp = props.targets[i].priority
  props.targets[i].priority = props.targets[j].priority
  props.targets[j].priority = tmp
}

function onProviderChange(t: TargetItem, value: string) {
  t.provider = value
  t.real_model = ""
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

    <div
      v-for="(t, i) in targets"
      :key="i"
      class="grid grid-cols-[auto_1fr_1fr_auto_auto] items-end gap-2 rounded-lg border p-3"
    >
      <span class="text-muted-foreground w-5 pt-5 text-center text-xs font-medium">
        {{ i + 1 }}
      </span>

      <div>
        <Label class="text-xs">Provider</Label>
        <Combobox
          :model-value="t.provider"
          :options="
            providers.map((p) => ({ value: p.name, label: `${p.name} (${p.provider_type})` }))
          "
          placeholder="Select provider..."
          search-placeholder="Search provider..."
          empty-text="No providers found"
          @update:model-value="(v) => onProviderChange(t, v as string)"
        />
      </div>

      <div>
        <Label class="text-xs">Real Model</Label>
        <Combobox
          v-model="t.real_model"
          :options="(providerModels[t.provider] ?? []).map((m) => ({ value: m }))"
          placeholder="Select model"
          search-placeholder="Search model..."
          empty-text="No models found"
          :loading="providerModelsLoading && !providerModels[t.provider]"
        />
      </div>

      <Popover>
        <PopoverTrigger as-child>
          <Button type="button" variant="outline" size="sm" class="gap-1 pt-5">
            <Settings2 class="h-3.5 w-3.5" />
            Configure
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
                @input="t.priority = Number(($event.target as HTMLInputElement).value)"
              />
            </div>
            <div>
              <Label class="text-xs">Retries</Label>
              <Input
                type="number"
                :model-value="t.retries"
                @input="t.retries = Number(($event.target as HTMLInputElement).value)"
              />
            </div>
            <div>
              <Label class="text-xs">Extra (JSON)</Label>
              <Textarea v-model="t.extra" placeholder='{"model_id": 39}' rows="4" />
            </div>
          </div>
        </PopoverContent>
      </Popover>

      <div class="flex items-center gap-0.5 pt-5">
        <template v-if="showPriorityControls">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            class="h-5 w-5"
            :disabled="i === 0"
            @click="swapPriority(i, i - 1)"
          >
            <ArrowUp class="h-3 w-3" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            class="h-5 w-5"
            :disabled="i === targets.length - 1"
            @click="swapPriority(i, i + 1)"
          >
            <ArrowDown class="h-3 w-3" />
          </Button>
        </template>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          :disabled="targets.length === 1"
          @click="removeTarget(i)"
        >
          <Trash2 class="h-4 w-4" />
        </Button>
      </div>
    </div>
  </div>
</template>
