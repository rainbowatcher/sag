<script setup lang="ts">
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import type { OAuthConfigForm } from "@/types/api"

const props = defineProps<{
  modelValue: OAuthConfigForm
  /** When true, `client_secret` is only displayed as `***` (stored secret). */
  secretIsMasked?: boolean
}>()

const emit = defineEmits<{
  "update:modelValue": [value: OAuthConfigForm]
}>()

function update<K extends keyof OAuthConfigForm>(key: K, value: OAuthConfigForm[K]) {
  emit("update:modelValue", { ...props.modelValue, [key]: value })
}
</script>

<template>
  <div class="col-span-2 space-y-3 rounded-md border p-3">
    <p class="text-muted-foreground text-sm">
      OAuth 2.0 client credentials. The gateway exchanges these at the token URL and injects
      <code class="text-xs">Authorization: Bearer</code> upstream.
    </p>

    <div class="grid grid-cols-2 gap-4">
      <div class="space-y-1">
        <FieldLabel>Token URL *</FieldLabel>
        <Input
          :model-value="modelValue.token_url"
          placeholder="https://login.example.com/oauth2/token"
          @input="update('token_url', ($event.target as HTMLInputElement).value)"
        />
      </div>

      <div class="space-y-1">
        <FieldLabel>Client ID *</FieldLabel>
        <Input
          :model-value="modelValue.client_id"
          placeholder="my-client-id"
          @input="update('client_id', ($event.target as HTMLInputElement).value)"
        />
      </div>

      <div class="space-y-1">
        <FieldLabel>Client Secret {{ secretIsMasked ? "(leave *** to keep)" : "*" }}</FieldLabel>
        <Input
          :model-value="modelValue.client_secret"
          type="password"
          :placeholder="secretIsMasked ? '***' : 'client-secret'"
          autocomplete="off"
          @input="update('client_secret', ($event.target as HTMLInputElement).value)"
        />
      </div>

      <div class="space-y-1">
        <FieldLabel>Scope</FieldLabel>
        <Input
          :model-value="modelValue.scope"
          placeholder="optional scope(s)"
          @input="update('scope', ($event.target as HTMLInputElement).value)"
        />
      </div>

      <div class="space-y-1">
        <FieldLabel>Audience</FieldLabel>
        <Input
          :model-value="modelValue.audience"
          placeholder="optional audience / resource"
          @input="update('audience', ($event.target as HTMLInputElement).value)"
        />
      </div>

      <div class="space-y-1">
        <FieldLabel>Refresh Token</FieldLabel>
        <Input
          :model-value="modelValue.refresh_token"
          placeholder="optional refresh_token grant"
          @input="update('refresh_token', ($event.target as HTMLInputElement).value)"
        />
      </div>
    </div>
  </div>
</template>
