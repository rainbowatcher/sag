<script setup lang="ts">
import { ref } from "vue"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import Combobox from "./Combobox.vue"

const countries = [
  { value: "cn", label: "China" },
  { value: "us", label: "United States" },
  { value: "jp", label: "Japan" },
  { value: "kr", label: "South Korea" },
  { value: "uk", label: "United Kingdom" },
  { value: "de", label: "Germany" },
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
]

const singleValue = ref("")
const multiValue = ref<string[]>([])
const loadingValue = ref("")
const emptyValue = ref("")
</script>

<template>
  <Story title="Combobox in Dialog" :layout="{ type: 'grid' }">
    <Variant title="Single Select">
      <Dialog>
        <DialogTrigger as-child>
          <Button>Open Dialog</Button>
        </DialogTrigger>
        <DialogContent class="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Single Select</DialogTitle>
          </DialogHeader>
          <div class="space-y-4 py-4">
            <p class="text-muted-foreground text-sm">Type to search and select one option:</p>
            <Combobox
              v-model="singleValue"
              :options="countries"
              placeholder="Select a country..."
              search-placeholder="Search countries..."
            />
          </div>
        </DialogContent>
      </Dialog>
    </Variant>

    <Variant title="Multi Select">
      <Dialog>
        <DialogTrigger as-child>
          <Button>Open Dialog</Button>
        </DialogTrigger>
        <DialogContent class="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Multi Select</DialogTitle>
          </DialogHeader>
          <div class="space-y-4 py-4">
            <p class="text-muted-foreground text-sm">
              Select multiple items — badges will collapse with overflow:
            </p>
            <Combobox
              v-model="multiValue"
              :options="tags"
              placeholder="Select tags..."
              search-placeholder="Search tags..."
              multiple
            />
          </div>
        </DialogContent>
      </Dialog>
    </Variant>

    <Variant title="Loading">
      <Dialog>
        <DialogTrigger as-child>
          <Button>Open Dialog</Button>
        </DialogTrigger>
        <DialogContent class="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Loading State</DialogTitle>
          </DialogHeader>
          <div class="space-y-4 py-4">
            <p class="text-muted-foreground text-sm">Combobox shown in loading state:</p>
            <Combobox
              v-model="loadingValue"
              :options="countries"
              placeholder="Loading..."
              search-placeholder="Search countries..."
              loading
            />
          </div>
        </DialogContent>
      </Dialog>
    </Variant>

    <Variant title="Empty">
      <Dialog>
        <DialogTrigger as-child>
          <Button>Open Dialog</Button>
        </DialogTrigger>
        <DialogContent class="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Empty Options</DialogTitle>
          </DialogHeader>
          <div class="space-y-4 py-4">
            <p class="text-muted-foreground text-sm">Combobox with no options:</p>
            <Combobox
              v-model="emptyValue"
              :options="[]"
              placeholder="Nothing to select..."
              search-placeholder="Search..."
              empty-text="No options available."
            />
          </div>
        </DialogContent>
      </Dialog>
    </Variant>
  </Story>
</template>
