<script setup lang="ts">
import { X } from "@lucide/vue"
import KvEditor from "@/components/KvEditor.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Separator } from "@/components/ui/separator"

interface KvPair {
  key: string
  value: string
}

const props = withDefaults(
  defineProps<{
    headers: KvPair[]
    bodyOverrides: KvPair[]
    stripFields: string[]
    roleMappings: KvPair[]
    showBodyOverrideWarning?: boolean
    /**
     * Role-mappings help text. The two forms ship slightly different copy, so
     * the caller supplies it to keep the rendered text unchanged.
     */
    roleMappingsDescription?: string
  }>(),
  {
    showBodyOverrideWarning: false,
    roleMappingsDescription:
      "Maps request role names to roles the upstream supports (case-sensitive). Example: developer → system.",
  },
)

const emit = defineEmits<{
  "update:headers": [value: KvPair[]]
  "update:bodyOverrides": [value: KvPair[]]
  "update:stripFields": [value: string[]]
  "update:roleMappings": [value: KvPair[]]
}>()

function updateStripField(idx: number, value: string) {
  const next = [...props.stripFields]
  next[idx] = value
  emit("update:stripFields", next)
}

function removeStripField(idx: number) {
  emit(
    "update:stripFields",
    props.stripFields.filter((_, i) => i !== idx),
  )
}

function addStripField() {
  emit("update:stripFields", [...props.stripFields, ""])
}
</script>

<template>
  <p class="text-muted-foreground text-xs">
    Provider-level defaults below apply to every API section. A field declared inside an
    <strong>APIs</strong> section <strong>fully replaces</strong> the default of the same name for
    that section (maps are not merged, arrays not unioned).
  </p>

  <KvEditor
    :model-value="headers"
    title="Headers"
    description="Key-value pairs of custom headers (values are plain text)."
    key-placeholder="Header-Name"
    value-placeholder="Value"
    @update:model-value="emit('update:headers', $event)"
  />

  <Separator />

  <Alert v-if="showBodyOverrideWarning" variant="warning">
    <AlertTitle>提示</AlertTitle>
    <AlertDescription>
      Body Overrides 对该供应商的<strong>所有模型</strong>生效。如需为特定模型设置不同参数，请前往
      <strong>APIs Tab</strong> 对应 API section 的 Advanced 中配置
      <strong>Model Overrides</strong>，避免在多个路由的 target 中重复配置 <code>extra</code>。
    </AlertDescription>
  </Alert>

  <KvEditor
    :model-value="bodyOverrides"
    title="Body Overrides"
    description='Key-value pairs overridden in the upstream JSON body. Values are parsed as JSON (e.g. <code>4096</code>, <code>true</code>, <code>"text"</code>).'
    key-placeholder="max_tokens"
    value-placeholder="4096"
    @update:model-value="emit('update:bodyOverrides', $event)"
  />

  <Separator />

  <div>
    <FieldLabel>Strip Fields</FieldLabel>
    <p class="text-muted-foreground mb-2 text-xs">
      Top-level fields to remove from the upstream JSON body before sending. Applied last, after all
      overrides. Use for fields the upstream rejects (e.g.
      <code>chat_template_kwargs</code> for pydantic <code>extra=forbid</code>).
    </p>
    <div v-for="(_, idx) in stripFields" :key="idx" class="mb-2 flex items-center gap-2">
      <Input
        :model-value="stripFields[idx]"
        placeholder="field_name"
        @input="updateStripField(idx, ($event.target as HTMLInputElement).value)"
      />
      <Button type="button" variant="outline" size="icon-sm" @click="removeStripField(idx)">
        <X class="h-3 w-3" />
      </Button>
    </div>
    <Button type="button" variant="outline" size="sm" @click="addStripField"> Add Field </Button>
  </div>

  <Separator />

  <KvEditor
    :model-value="roleMappings"
    title="Role Mappings"
    :description="roleMappingsDescription"
    key-placeholder="developer"
    value-placeholder="system"
    @update:model-value="emit('update:roleMappings', $event)"
  />
</template>
