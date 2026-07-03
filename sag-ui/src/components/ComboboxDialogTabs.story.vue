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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import Combobox from "./Combobox.vue"

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

const selectedTags = ref<string[]>([])
</script>

<template>
  <Story title="Combobox in Dialog+Tabs" :layout="{ type: 'single', iframe: false }">
    <Variant title="Dialog > Tabs > Multi Combobox">
      <Dialog>
        <DialogTrigger as-child>
          <Button>Open Dialog</Button>
        </DialogTrigger>
        <DialogContent class="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Provider Configuration</DialogTitle>
          </DialogHeader>
          <Tabs default-value="models" class="py-4">
            <TabsList>
              <TabsTrigger value="basic">Basic</TabsTrigger>
              <TabsTrigger value="models">Models</TabsTrigger>
            </TabsList>
            <TabsContent value="basic" class="space-y-4 pt-4">
              <p class="text-muted-foreground text-sm">Basic settings go here.</p>
            </TabsContent>
            <TabsContent value="models" class="space-y-4 pt-4">
              <p class="text-muted-foreground text-sm">Select models:</p>
              <Combobox
                v-model="selectedTags"
                :options="tags"
                placeholder="Select models..."
                search-placeholder="Search models..."
                multiple
              />
            </TabsContent>
          </Tabs>
        </DialogContent>
      </Dialog>
    </Variant>
  </Story>
</template>
