<script setup lang="ts">
import { reactive } from "vue"
import Combobox from "./Combobox.vue"

const countries = [
  { value: "cn", label: "China" },
  { value: "us", label: "United States" },
  { value: "jp", label: "Japan" },
  { value: "kr", label: "South Korea" },
  { value: "uk", label: "United Kingdom" },
  { value: "de", label: "Germany" },
  { value: "fr", label: "France" },
  { value: "sg", label: "Singapore" },
]

const tags = [
  { value: "frontend", label: "Frontend" },
  { value: "backend", label: "Backend" },
  { value: "devops", label: "DevOps" },
  { value: "design", label: "Design" },
  { value: "mobile", label: "Mobile" },
  { value: "ai", label: "AI / ML" },
  { value: "security", label: "Security" },
  { value: "database", label: "Database" },
  { value: "testing", label: "Testing" },
  { value: "docs", label: "Documentation" },
]

const emptyOptions: { value: string; label?: string }[] = []

// ── Single-select state ──
const singleState = reactive<{ value: string | string[] }>({ value: "" })
// ── Multi-select state ──
const multiState = reactive<{ value: string | string[] }>({ value: [] })
// ── Loading state ──
const loadingState = reactive<{ value: string | string[] }>({ value: "" })
</script>

<template>
  <Story title="Combobox" :layout="{ type: 'single', iframe: false }">
    <Variant title="Single Select">
      <Combobox
        v-model="singleState.value"
        :options="countries"
        placeholder="Select a country..."
        search-placeholder="Search countries..."
      />
      <template #controls>
        <HstSelect
          v-model="singleState.value"
          title="Selected"
          :options="countries.map((c) => ({ value: c.value, label: c.label }))"
        />
      </template>
    </Variant>

    <Variant title="Multi Select">
      <Combobox
        v-model="multiState.value"
        :options="tags"
        placeholder="Select tags..."
        search-placeholder="Search tags..."
        multiple
      />
      <template #controls>
        <HstTags
          v-model="multiState.value"
          title="Selected Tags"
          :options="tags.map((t) => ({ value: t.value, label: t.label }))"
        />
      </template>
    </Variant>

    <Variant title="Empty Options">
      <Combobox
        v-model="singleState.value"
        :options="emptyOptions"
        placeholder="No options available"
        empty-text="There are no items to select."
      />
    </Variant>

    <Variant title="Loading State">
      <Combobox
        v-model="loadingState.value"
        :options="countries"
        placeholder="Loading..."
        loading
      />
    </Variant>
  </Story>
</template>
