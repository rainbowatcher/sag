<script setup lang="ts">
/**
 * Live in-flight request badges for the routes page.
 *
 * Sits above the route table and mirrors the `GET /api/inflight` SSE stream:
 * the total on the left, one `model_name ×n` badge per route that currently has
 * traffic, and an "idle" placeholder when nothing is in flight. When the stream
 * drops the last snapshot is kept but dimmed behind a "reconnecting" label — a
 * dead stream must never render as "nothing in flight".
 */
import { Activity, Info, WifiOff } from "@lucide/vue"
import { computed } from "vue"
import { Badge } from "@/components/ui/badge"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { useInflightStore } from "@/stores/inflight"

const inflightStore = useInflightStore()

/** Badges only exist for routes with traffic (store already filters + sorts). */
const routes = computed(() => inflightStore.activeRoutes)

/** Live traffic or not; a stale snapshot still counts as traffic. */
const hasTraffic = computed(() => inflightStore.hasTraffic)

/** Dimmed rendering while the stream is down and a snapshot is held. */
const stale = computed(() => inflightStore.stale)

/**
 * The snapshot confirms there is nothing in flight. Both the route list and the
 * total must say so: if the two disagree (a route entry dropped as malformed),
 * rendering "idle" next to "processing N" would contradict itself.
 */
const idle = computed(
  () => inflightStore.snapshot?.routes.length === 0 && inflightStore.total === 0 && !stale.value,
)
</script>

<template>
  <!-- Nothing is known before the first snapshot: no row beats a row that claims "0". -->
  <div v-if="inflightStore.snapshot !== null" class="flex flex-wrap items-center gap-x-3 gap-y-2">
    <span class="flex shrink-0 items-center gap-1.5 text-xs font-medium">
      <Activity
        class="h-3.5 w-3.5"
        :class="hasTraffic && !stale ? 'animate-pulse text-green-500' : 'text-muted-foreground'"
      />
      {{ $t("routeActivity.processing", { count: inflightStore.total }) }}
      <Tooltip>
        <TooltipTrigger as-child>
          <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
        </TooltipTrigger>
        <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
          {{ $t("routeActivity.processingTitle") }}
        </TooltipContent>
      </Tooltip>
    </span>

    <!-- ── Stale / disconnected ───────────────────────────────────── -->
    <Tooltip v-if="stale">
      <TooltipTrigger as-child>
        <span class="text-muted-foreground flex cursor-help items-center gap-1 text-xs">
          <WifiOff class="h-3.5 w-3.5" />
          {{ $t("routeActivity.reconnecting") }}
        </span>
      </TooltipTrigger>
      <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
        {{ $t("routeActivity.staleHint") }}
      </TooltipContent>
    </Tooltip>

    <span v-if="hasTraffic" class="text-muted-foreground text-xs">·</span>

    <!-- ── Per-route badges ──────────────────────────────────────── -->
    <div v-if="hasTraffic" class="flex min-w-0 flex-wrap items-center gap-1.5">
      <Tooltip v-for="route in routes" :key="route.model_name">
        <TooltipTrigger as-child>
          <Badge
            variant="outline"
            class="cursor-help gap-1.5 font-normal"
            :class="stale ? 'opacity-60' : ''"
          >
            <span
              class="h-1.5 w-1.5 shrink-0 rounded-full"
              :class="stale ? 'bg-muted-foreground' : 'animate-pulse bg-green-500'"
            />
            <span class="max-w-[16rem] truncate font-mono">{{ route.model_name }}</span>
            <span class="font-mono tabular-nums">×{{ route.count }}</span>
          </Badge>
        </TooltipTrigger>
        <TooltipContent side="top" class="font-mono text-xs tabular-nums">
          {{ $t("routeActivity.routeTitle", { model: route.model_name, count: route.count }) }}
        </TooltipContent>
      </Tooltip>
    </div>

    <!-- ── Idle placeholder: a snapshot confirmed zero traffic ──────── -->
    <span v-if="idle" class="text-muted-foreground text-xs">
      {{ $t("routeActivity.idle") }}
    </span>
  </div>
</template>
