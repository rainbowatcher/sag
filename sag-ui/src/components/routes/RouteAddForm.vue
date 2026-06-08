<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import {
  X,
  Loader2,
  ChevronsUpDown,
  Check,
  ArrowUp,
  ArrowDown,
  Plus,
  Trash2,
} from "lucide-vue-next"
import { ref } from "vue"
import { z } from "zod"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command"
import { Field, FieldError, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { useRouteStore } from "@/stores/routes"
import type { AddRoutePayload, Provider, Strategy } from "@/types/api"

const routeStore = useRouteStore()

const props = defineProps<{
  providers: Provider[]
  providerModelsLoading: boolean
  providerModels: Record<string, string[]>
}>()

const emit = defineEmits<{
  close: []
  created: []
}>()

const openPopover = ref<Record<number, boolean>>({})
const adding = ref(false)
const apiError = ref("")

interface TargetForm {
  provider_name: string
  real_model: string
  priority: number
}

const targets = ref<TargetForm[]>([{ provider_name: "", real_model: "", priority: 0 }])

function addTarget() {
  targets.value.push({ provider_name: "", real_model: "", priority: 0 })
}

function removeTarget(index: number) {
  if (targets.value.length > 1) {
    targets.value.splice(index, 1)
  }
}

function swapPriority(i: number, j: number) {
  if (j < 0 || j >= targets.value.length) return
  const tmp = targets.value[i].priority
  targets.value[i].priority = targets.value[j].priority
  targets.value[j].priority = tmp
}

const endpointPresets = [
  { label: "Chat Completions", value: "/v1/chat/completions" },
  { label: "Responses (GPT-5)", value: "/v1/responses" },
  { label: "Image Generation", value: "/v1/images/generations" },
]

const formSchema = z.object({
  model_name: z.string(),
  api_endpoint: z.string().min(1),
  strategy: z.enum(["fill_first", "round_robin"]),
})

const form = useForm({
  defaultValues: {
    model_name: "",
    api_endpoint: "/v1/chat/completions",
    strategy: "fill_first" as Strategy,
  },
  validators: {
    onSubmit: formSchema,
  },
})

async function handleSubmit(event: Event) {
  event.preventDefault()
  apiError.value = ""
  adding.value = true
  try {
    await form.handleSubmit()
    if (!form.state.isSubmitted) return
    if (apiError.value) return

    if (targets.value.length === 0) {
      apiError.value = "At least one target is required"
      return
    }
    const emptyTarget = targets.value.find((t) => !t.provider_name)
    if (emptyTarget) {
      apiError.value = "Each target must have a provider selected"
      return
    }

    const routeName = form.state.values.model_name || targets.value[0].real_model || ""
    if (!routeName) {
      apiError.value = "Please provide model_name or fill real_model on the first target"
      return
    }

    const errors: string[] = []
    for (const [i, t] of targets.value.entries()) {
      const payload: AddRoutePayload = {
        model_name: routeName,
        real_model: t.real_model || undefined,
        api_endpoint: form.state.values.api_endpoint || undefined,
        priority: t.priority,
        strategy: i === 0 ? form.state.values.strategy : undefined,
      }
      try {
        await routeStore.addRoute(t.provider_name, payload)
      } catch (e: any) {
        errors.push(`${t.provider_name}: ${e.response?.data?.error?.message ?? e.message}`)
      }
    }

    if (errors.length > 0) {
      apiError.value = errors.join("; ")
      return
    }
    emit("created")
  } catch (e: any) {
    apiError.value = e.response?.data?.error?.message ?? e.message
  } finally {
    adding.value = false
  }
}
</script>

<template>
  <Card class="space-y-4">
    <CardHeader class="flex flex-row items-center justify-between space-y-0">
      <CardTitle>New Route</CardTitle>
      <Button variant="ghost" size="icon-sm" @click="emit('close')">
        <X class="h-4 w-4" />
      </Button>
    </CardHeader>
    <CardContent>
      <div v-if="apiError" class="mb-4 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
        {{ apiError }}
      </div>
      <form id="route-add-form" @submit.prevent="handleSubmit">
        <div class="grid grid-cols-2 gap-4">
          <form.Field name="model_name" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">Route Alias</FieldLabel>
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="gpt-4o"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <p class="text-muted-foreground text-[10px]">
                Leave empty to use the first target's real_model
              </p>
            </Field>
          </form.Field>

          <form.Field name="strategy" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">Strategy</FieldLabel>
              <ToggleGroup
                :model-value="field.state.value"
                @update:model-value="(v) => field.handleChange(v as Strategy)"
                type="single"
                variant="outline"
                size="sm"
                class="justify-start"
              >
                <ToggleGroupItem value="fill_first">Fill-first</ToggleGroupItem>
                <ToggleGroupItem value="round_robin">Round-robin</ToggleGroupItem>
              </ToggleGroup>
            </Field>
          </form.Field>

          <div class="col-span-2">
            <form.Field name="api_endpoint" #default="{ field }">
              <Field>
                <FieldLabel :for="field.name">API Endpoint</FieldLabel>
                <ToggleGroup
                  :model-value="field.state.value"
                  @update:model-value="(v) => field.handleChange(v as string)"
                  type="single"
                  variant="outline"
                  size="sm"
                  class="justify-start"
                >
                  <ToggleGroupItem v-for="ep in endpointPresets" :key="ep.value" :value="ep.value">
                    {{ ep.label }}
                  </ToggleGroupItem>
                </ToggleGroup>
              </Field>
            </form.Field>
          </div>
        </div>

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
            class="flex items-end gap-2 rounded-lg border p-3"
          >
            <span class="text-muted-foreground mb-2 w-5 pt-1 text-center text-xs font-medium">
              {{ i + 1 }}
            </span>

            <div class="flex-1">
              <Label class="text-xs">Provider</Label>
              <Select
                :model-value="t.provider_name"
                @update:model-value="
                  (v) => {
                    t.provider_name = v as string
                    routeStore.fetchProviderModels(v as string)
                  }
                "
              >
                <SelectTrigger>
                  <SelectValue placeholder="Select..." />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem v-for="p in providers" :key="p.name" :value="p.name">
                    {{ p.name }} ({{ p.provider_type }})
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div class="flex-1">
              <Label class="text-xs">Real Model</Label>
              <Popover v-model:open="openPopover[i]">
                <PopoverTrigger as-child>
                  <Button
                    variant="outline"
                    role="combobox"
                    :aria-expanded="openPopover[i]"
                    class="w-full justify-between"
                  >
                    {{ t.real_model || "Select model" }}
                    <ChevronsUpDown class="ml-auto h-4 w-4 shrink-0 opacity-50" />
                  </Button>
                </PopoverTrigger>
                <PopoverContent class="w-(--reka-popper-anchor-width) p-0">
                  <Command>
                    <CommandInput placeholder="Search model..." />
                    <CommandList>
                      <CommandEmpty>No models found</CommandEmpty>
                      <div
                        v-if="providerModelsLoading && !providerModels[t.provider_name]"
                        class="text-muted-foreground px-2 py-4 text-center text-xs"
                      >
                        Loading models...
                      </div>
                      <CommandGroup>
                        <CommandItem
                          v-for="m in providerModels[t.provider_name] ?? []"
                          :key="m"
                          :value="m"
                          @select="
                            (ev) => {
                              const val = ev.detail.value as string
                              t.real_model = val === t.real_model ? '' : val
                              openPopover[i] = false
                            }
                          "
                        >
                          {{ m }}
                          <Check
                            :class="t.real_model === m ? 'opacity-100' : 'opacity-0'"
                            class="ml-auto h-4 w-4"
                          />
                        </CommandItem>
                      </CommandGroup>
                    </CommandList>
                  </Command>
                </PopoverContent>
              </Popover>
            </div>

            <div class="w-20">
              <Label class="text-xs">Priority</Label>
              <Input
                type="number"
                :model-value="t.priority"
                @input="t.priority = Number(($event.target as HTMLInputElement).value)"
              />
            </div>

            <div class="flex flex-col gap-0.5 pb-1">
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
            </div>

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
      </form>
    </CardContent>
    <CardFooter class="flex justify-end">
      <Button type="submit" form="route-add-form" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? "Adding..." : "Create Route" }}
      </Button>
    </CardFooter>
  </Card>
</template>
