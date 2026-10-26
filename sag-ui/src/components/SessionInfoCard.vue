<script setup lang="ts">
/**
 * Session registry details for the active `?session=` log filter.
 *
 * The backend joins `GET /api/logs?session=<key>` against the
 * `session_registry` table and returns the display facts for that hashed key
 * (`session_source`, the raw conversation id for `meta` / `top` sources and a
 * first-message preview). The store hands that record over as
 * `logsSession`, which is `null` both when no session filter is applied and
 * when the key has no registry row — the card keeps rendering the key itself
 * and explains the missing details instead of disappearing. The page only
 * forwards the record when `logsSessionKey` still matches the filter being
 * displayed, so a freshly switched (or failed) filter degrades to "no
 * details" rather than showing the previous key's row.
 */
import { computed } from "vue"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { shortSessionKey } from "@/lib/sessionAffinity"
import {
  SESSION_PLACEHOLDER,
  formatSessionTime,
  sessionPreviewText,
  sessionSourceKind,
  sessionText,
} from "@/lib/sessionInfo"
import type { SessionInfo } from "@/types/api"

const props = defineProps<{
  /** Registry row for the filtered key; `null` when unfiltered or unregistered. */
  session: SessionInfo | null
  /** The hashed key currently applied as the session filter. */
  sessionKey: string
}>()

/** Short form of the hash; the full 32-hex key stays in `title`. */
const shortKey = computed(() => shortSessionKey(props.sessionKey))

/** Localized label for the extraction-chain source, or the raw value if unknown. */
const sourceLabel = computed(() => {
  switch (sessionSourceKind(props.session?.session_source)) {
    case "meta":
      return t("logs.sessionCard.source.meta")
    case "top":
      return t("logs.sessionCard.source.top")
    case "pc":
      return t("logs.sessionCard.source.pc")
    case "fb":
      return t("logs.sessionCard.source.fb")
    default:
      return props.session?.session_source?.trim() || SESSION_PLACEHOLDER
  }
})

/** Raw conversation id (`meta` / `top` only) as a single-line value. */
const conversationId = computed(() => sessionText(props.session?.conversation_id))

/** Title-style preview of the session's first user message. */
const preview = computed(() => sessionPreviewText(props.session?.first_message_preview))

const firstSeen = computed(() => formatSessionTime(props.session?.first_seen))
const lastSeen = computed(() => formatSessionTime(props.session?.last_seen))
</script>

<template>
  <Card class="gap-2 py-3">
    <CardHeader class="px-4 py-0">
      <CardTitle class="text-sm">{{ $t("logs.sessionCard.title") }}</CardTitle>
    </CardHeader>
    <CardContent class="px-4">
      <dl
        class="grid grid-cols-1 gap-x-6 gap-y-2 text-xs sm:grid-cols-2"
        :class="props.session ? 'xl:grid-cols-4' : ''"
      >
        <div class="min-w-0 space-y-0.5">
          <dt class="text-muted-foreground text-[10px] tracking-wide uppercase">
            {{ $t("logs.sessionCard.sessionKey") }}
          </dt>
          <dd class="truncate font-mono" :title="props.sessionKey">{{ shortKey }}</dd>
        </div>
        <template v-if="props.session">
          <div class="min-w-0 space-y-0.5">
            <dt class="text-muted-foreground text-[10px] tracking-wide uppercase">
              {{ $t("logs.sessionCard.source") }}
            </dt>
            <dd>
              <Tooltip>
                <TooltipTrigger as-child>
                  <Badge variant="outline" class="cursor-help text-[10px]">{{ sourceLabel }}</Badge>
                </TooltipTrigger>
                <TooltipContent side="top" class="font-mono text-xs tabular-nums">
                  <div>{{ $t("logs.sessionCard.firstSeen") }}: {{ firstSeen }}</div>
                  <div>{{ $t("logs.sessionCard.lastSeen") }}: {{ lastSeen }}</div>
                </TooltipContent>
              </Tooltip>
            </dd>
          </div>
          <div class="min-w-0 space-y-0.5">
            <dt class="text-muted-foreground text-[10px] tracking-wide uppercase">
              {{ $t("logs.sessionCard.conversationId") }}
            </dt>
            <dd
              class="truncate font-mono"
              :class="conversationId === SESSION_PLACEHOLDER ? 'text-muted-foreground' : ''"
              :title="props.session.conversation_id ?? undefined"
            >
              {{ conversationId }}
            </dd>
          </div>
          <div class="min-w-0 space-y-0.5">
            <dt class="text-muted-foreground text-[10px] tracking-wide uppercase">
              {{ $t("logs.sessionCard.preview") }}
            </dt>
            <dd
              class="truncate"
              :class="preview === SESSION_PLACEHOLDER ? 'text-muted-foreground' : ''"
              :title="props.session.first_message_preview ?? undefined"
            >
              {{ preview }}
            </dd>
          </div>
        </template>
      </dl>
      <p v-if="!props.session" class="text-muted-foreground mt-2 text-xs">
        {{ $t("logs.sessionCard.noRecord") }}
      </p>
    </CardContent>
  </Card>
</template>
