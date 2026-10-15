<script lang="ts">
/**
 * One read-only key/value line of a [`SystemInfoCard`](SystemInfoCard.vue).
 * `value` may be missing when the corresponding section is absent from the
 * `GET /api/config` snapshot — the card then renders a muted placeholder.
 */
export interface SystemInfoRow {
  label: string
  value: string | number | null | undefined
  /** Render the value in a monospace font (addresses, DSNs, log levels). */
  mono?: boolean
}
</script>

<script setup lang="ts">
import { computed } from "vue"
import type { Component } from "vue"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { t } from "@/lib/i18n"

const props = withDefaults(
  defineProps<{
    title: string
    description?: string
    /**
     * Header badge. Defaults to "Requires restart": every section rendered by
     * this card (`server` / `logging` / `analytics`) applies at process
     * startup and is not writable through the API.
     */
    badge?: string
    icon?: Component
    rows: SystemInfoRow[]
  }>(),
  { description: "" },
)

/** Header badge, defaulting to the localized "requires restart" note. */
const badgeText = computed(() => props.badge ?? t("settings.info.requiresRestart"))

/** A missing section field renders as a placeholder instead of an empty cell. */
function isMissing(value: SystemInfoRow["value"]): boolean {
  return value === null || value === undefined || value === ""
}

function display(value: SystemInfoRow["value"]): string {
  return isMissing(value) ? "—" : String(value)
}

function rowClass(row: SystemInfoRow): string {
  return [row.mono ? "font-mono" : "", isMissing(row.value) ? "text-muted-foreground italic" : ""]
    .filter(Boolean)
    .join(" ")
}
</script>

<template>
  <Card>
    <CardHeader>
      <div class="flex flex-wrap items-start justify-between gap-3">
        <div class="max-w-3xl min-w-0 space-y-1.5">
          <CardTitle class="flex items-center gap-2">
            <component :is="props.icon" v-if="props.icon" class="h-4 w-4" />
            {{ props.title }}
          </CardTitle>
          <CardDescription v-if="props.description">{{ props.description }}</CardDescription>
        </div>
        <Badge v-if="badgeText" variant="outline" class="shrink-0 text-[10px]">
          {{ badgeText }}
        </Badge>
      </div>
    </CardHeader>
    <CardContent>
      <dl v-if="props.rows.length > 0" class="max-w-3xl divide-y overflow-hidden rounded-md border">
        <div
          v-for="row in props.rows"
          :key="row.label"
          class="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 px-3 py-2"
        >
          <dt class="text-muted-foreground text-xs">{{ row.label }}</dt>
          <dd class="text-right text-xs break-all" :class="rowClass(row)">
            {{ display(row.value) }}
          </dd>
        </div>
      </dl>
      <p v-else class="text-muted-foreground text-xs">
        {{ $t("settings.info.sectionMissing") }}
      </p>
    </CardContent>
  </Card>
</template>
