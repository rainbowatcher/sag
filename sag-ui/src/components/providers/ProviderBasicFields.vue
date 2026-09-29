<script setup lang="ts">
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import ProviderOAuthFields from "@/components/providers/ProviderOAuthFields.vue"
import { FieldLabel } from "@/components/ui/field"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import type { AuthMode, OAuthConfigForm, ProxyMode } from "@/types/api"

const props = defineProps<{
  apiKeys: string[]
  keyStrategy: "fill_first" | "round_robin"
  proxyMode: ProxyMode
  authMode: AuthMode
  oauth: OAuthConfigForm
  /** Edit-form only: stored secret is shown as `***` and kept on save. */
  secretIsMasked?: boolean
}>()

const emit = defineEmits<{
  "update:apiKeys": [value: string[]]
  "update:keyStrategy": [value: "fill_first" | "round_robin"]
  "update:proxyMode": [value: ProxyMode]
  "update:authMode": [value: AuthMode]
  "update:oauth": [value: OAuthConfigForm]
}>()

const isOAuth = () => props.authMode === "oauth"
</script>

<template>
  <div class="space-y-1">
    <FieldLabel>Auth Mode</FieldLabel>
    <Select
      :model-value="authMode"
      @update:model-value="(v) => emit('update:authMode', v as AuthMode)"
    >
      <SelectTrigger>
        <SelectValue />
      </SelectTrigger>
      <SelectContent class="w-(--reka-select-trigger-width)">
        <SelectItem value="static">Static API Keys</SelectItem>
        <SelectItem value="oauth">OAuth 2.0 Client Credentials</SelectItem>
      </SelectContent>
    </Select>
    <p class="text-muted-foreground text-xs">
      Static uses <code class="text-xs">api_keys</code>; OAuth exchanges client credentials at the
      token URL and ignores the key pool.
    </p>
  </div>

  <div class="space-y-1">
    <FieldLabel>Key Strategy</FieldLabel>
    <Select
      :model-value="keyStrategy"
      :disabled="isOAuth()"
      @update:model-value="(v) => emit('update:keyStrategy', v as 'fill_first' | 'round_robin')"
    >
      <SelectTrigger>
        <SelectValue />
      </SelectTrigger>
      <SelectContent class="w-(--reka-select-trigger-width)">
        <SelectItem value="fill_first">Fill First</SelectItem>
        <SelectItem value="round_robin">Round Robin</SelectItem>
      </SelectContent>
    </Select>
  </div>

  <div class="space-y-1">
    <FieldLabel>Proxy Mode</FieldLabel>
    <Select
      :model-value="proxyMode"
      @update:model-value="(v) => emit('update:proxyMode', v as ProxyMode)"
    >
      <SelectTrigger>
        <SelectValue />
      </SelectTrigger>
      <SelectContent class="w-(--reka-select-trigger-width)">
        <SelectItem value="inherit">Inherit</SelectItem>
        <SelectItem value="enabled">Enabled</SelectItem>
        <SelectItem value="disabled">Disabled</SelectItem>
      </SelectContent>
    </Select>
    <p class="text-muted-foreground text-xs">
      How this provider resolves the upstream proxy. Inherit follows the gateway-level proxy URL.
    </p>
  </div>

  <div class="col-span-2" :class="{ 'opacity-50': isOAuth() }">
    <ApiKeyEditor
      :api-keys="apiKeys"
      :disabled="isOAuth()"
      @update:api-keys="emit('update:apiKeys', $event)"
    />
  </div>

  <ProviderOAuthFields
    v-if="isOAuth()"
    :model-value="oauth"
    :secret-is-masked="secretIsMasked"
    @update:model-value="emit('update:oauth', $event)"
  />
</template>
