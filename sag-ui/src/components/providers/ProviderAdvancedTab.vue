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
    roleMappingsDescription: undefined,
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
    {{ $t("providers.advanced.introLead")
    }}<strong>{{ $t("providers.advanced.introApisTab") }}</strong
    >{{ $t("providers.advanced.introMid")
    }}<strong>{{ $t("providers.advanced.introFullyReplaces") }}</strong
    >{{ $t("providers.advanced.introTail") }}
  </p>

  <KvEditor
    :model-value="headers"
    :title="$t('providers.advanced.headersTitle')"
    :description="$t('providers.advanced.headersDescription')"
    :key-placeholder="$t('providers.advanced.headerKeyPlaceholder')"
    :value-placeholder="$t('common.value')"
    @update:model-value="emit('update:headers', $event)"
  />

  <Separator />

  <Alert v-if="showBodyOverrideWarning" variant="warning">
    <AlertTitle>{{ $t("providers.advanced.bodyOverrideWarningTitle") }}</AlertTitle>
    <AlertDescription>
      {{ $t("providers.advanced.bodyOverrideWarningLead")
      }}<strong>{{ $t("providers.advanced.bodyOverrideWarningAllModels") }}</strong
      >{{ $t("providers.advanced.bodyOverrideWarningAfterAllModels")
      }}<strong>{{ $t("providers.advanced.bodyOverrideWarningApisTab") }}</strong
      >{{ $t("providers.advanced.bodyOverrideWarningAfterApisTab")
      }}<strong>{{ $t("providers.advanced.bodyOverrideWarningModelOverrides") }}</strong
      >{{ $t("providers.advanced.bodyOverrideWarningAfterModelOverrides") }}<code>extra</code
      >{{ $t("providers.advanced.bodyOverrideWarningEnd") }}
    </AlertDescription>
  </Alert>

  <KvEditor
    :model-value="bodyOverrides"
    :title="$t('providers.advanced.bodyOverridesTitle')"
    :description="$t('providers.advanced.bodyOverridesDescription')"
    key-placeholder="max_tokens"
    value-placeholder="4096"
    @update:model-value="emit('update:bodyOverrides', $event)"
  />

  <Separator />

  <div>
    <FieldLabel>{{ $t("providers.advanced.stripFieldsTitle") }}</FieldLabel>
    <p class="text-muted-foreground mb-2 text-xs">
      {{ $t("providers.advanced.stripFieldsDescription") }}
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
    <Button type="button" variant="outline" size="sm" @click="addStripField">
      {{ $t("providers.advanced.addField") }}
    </Button>
  </div>

  <Separator />

  <KvEditor
    :model-value="roleMappings"
    :title="$t('providers.advanced.roleMappingsTitle')"
    :description="
      roleMappingsDescription ?? $t('providers.advanced.roleMappingsDescriptionDefault')
    "
    key-placeholder="developer"
    value-placeholder="system"
    @update:model-value="emit('update:roleMappings', $event)"
  />
</template>
