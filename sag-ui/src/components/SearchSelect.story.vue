<script setup lang="ts">
import { reactive } from "vue"
import SearchSelect from "./SearchSelect.vue"

const modelTags = [
  { value: "gpt-4o", label: "GPT-4o" },
  { value: "gpt-4o-mini", label: "GPT-4o Mini" },
  { value: "o1", label: "o1" },
  { value: "o1-mini", label: "o1 Mini" },
  { value: "claude-3-7-sonnet", label: "Claude 3.7 Sonnet" },
  { value: "claude-3-5-haiku", label: "Claude 3.5 Haiku" },
  { value: "gemini-2-0-flash", label: "Gemini 2.0 Flash" },
  { value: "gemini-2-5-pro", label: "Gemini 2.5 Pro" },
  { value: "deepseek-r1", label: "DeepSeek R1" },
  { value: "deepseek-v3", label: "DeepSeek V3" },
  { value: "qwen-turbo", label: "Qwen Turbo" },
  { value: "llama-3-3-70b", label: "Llama 3.3 70B" },
]

const emptyOptions: { value: string; label?: string }[] = []

// ── Single Select state ──
const singleState = reactive<{ value: string }>({ value: "" })
const singlePreSelectedState = reactive<{ value: string }>({ value: "gpt-4o" })

// ── Default: empty selected state ──
const defaultState = reactive<{ value: string[] }>({ value: [] })

// ── Pre-selected: 3 items ──
const preSelectedState = reactive<{ value: string[] }>({
  value: ["gpt-4o", "claude-3-7-sonnet", "deepseek-r1"],
})

// ── Overflow Tags: 6+ items ──
const overflowState = reactive<{ value: string[] }>({
  value: [
    "gpt-4o",
    "gpt-4o-mini",
    "claude-3-7-sonnet",
    "claude-3-5-haiku",
    "gemini-2-0-flash",
    "deepseek-r1",
    "deepseek-v3",
  ],
})

// ── Disabled state ──
const disabledState = reactive<{ value: string[]; disabled: boolean }>({
  value: ["gpt-4o", "claude-3-7-sonnet"],
  disabled: true,
})

// ── Hide Clear All state ──
const hideClearAllState = reactive<{ value: string[]; hideClearAll: boolean }>({
  value: ["gpt-4o", "deepseek-r1", "llama-3-3-70b"],
  hideClearAll: true,
})

// ── Custom Placeholder state ──
const customPlaceholderState = reactive<{ value: string[] }>({ value: [] })
</script>

<template>
  <Story title="SearchSelect" :layout="{ type: 'single', iframe: false }">
    <Variant title="Single Select">
      <SearchSelect
        v-model="singleState.value"
        :options="modelTags"
        placeholder="Select a model..."
        search-placeholder="Search models..."
      />
      <template #controls>
        <HstText v-model="singleState.value" title="Selected Model" />
      </template>
    </Variant>

    <Variant title="Single Select Pre-selected">
      <SearchSelect
        v-model="singlePreSelectedState.value"
        :options="modelTags"
        placeholder="Select a model..."
        search-placeholder="Search models..."
      />
      <template #controls>
        <HstText v-model="singlePreSelectedState.value" title="Selected Model" />
      </template>
    </Variant>

    <Variant title="Default">
      <SearchSelect
        v-model="defaultState.value"
        :options="modelTags"
        placeholder="Select AI models..."
        search-placeholder="Search models..."
        multiple
      />
      <template #controls>
        <HstTags
          v-model="defaultState.value"
          title="Selected Models"
          :options="modelTags.map((t) => ({ value: t.value, label: t.label }))"
        />
      </template>
    </Variant>

    <Variant title="Pre-selected">
      <SearchSelect
        v-model="preSelectedState.value"
        :options="modelTags"
        placeholder="Select AI models..."
        search-placeholder="Search models..."
        multiple
      />
      <template #controls>
        <HstTags
          v-model="preSelectedState.value"
          title="Selected Models"
          :options="modelTags.map((t) => ({ value: t.value, label: t.label }))"
        />
      </template>
    </Variant>

    <Variant title="Overflow Tags">
      <div class="max-w-sm">
        <SearchSelect
          v-model="overflowState.value"
          :options="modelTags"
          placeholder="Select AI models..."
          search-placeholder="Search models..."
          multiple
        />
      </div>
      <template #controls>
        <HstTags
          v-model="overflowState.value"
          title="Selected Models"
          :options="modelTags.map((t) => ({ value: t.value, label: t.label }))"
        />
      </template>
    </Variant>

    <Variant title="Disabled">
      <SearchSelect
        v-model="disabledState.value"
        :options="modelTags"
        placeholder="Select AI models..."
        search-placeholder="Search models..."
        :disabled="disabledState.disabled"
        multiple
      />
      <template #controls>
        <HstCheckbox v-model="disabledState.disabled" title="Disabled" />
      </template>
    </Variant>

    <Variant title="Hide Clear All">
      <SearchSelect
        v-model="hideClearAllState.value"
        :options="modelTags"
        placeholder="Select AI models..."
        search-placeholder="Search models..."
        :hide-clear-all="hideClearAllState.hideClearAll"
        multiple
      />
      <template #controls>
        <HstCheckbox v-model="hideClearAllState.hideClearAll" title="Hide Clear All" />
      </template>
    </Variant>

    <Variant title="Empty Options">
      <SearchSelect
        v-model="singleState.value"
        :options="emptyOptions"
        placeholder="Select AI models..."
        empty-text="No models available."
      />
    </Variant>

    <Variant title="Custom Placeholder">
      <SearchSelect
        v-model="customPlaceholderState.value"
        :options="modelTags"
        placeholder="Pick your models"
        search-placeholder="Type to find models..."
        multiple
      />
    </Variant>
  </Story>
</template>
