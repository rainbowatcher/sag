import { useMediaQuery } from "@vueuse/core"
import { computed } from "vue"
import type { Component } from "vue"
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import {
  Drawer,
  DrawerClose,
  DrawerContent,
  DrawerDescription,
  DrawerFooter,
  DrawerHeader,
  DrawerTitle,
  DrawerTrigger,
} from "@/components/ui/drawer"

/**
 * Mobile breakpoint. Kept in sync with `AppLayout.vue` / `SidebarProvider`'s
 * `(max-width: 768px)` so the whole shell flips to its mobile layout at the
 * same viewport width as these modals flip to drawers.
 */
export const MODAL_DESKTOP_QUERY = "(min-width: 768px)"

export interface ModalComponents {
  Root: Component
  Content: Component
  Header: Component
  Title: Component
  Description: Component
  Footer: Component
  Close: Component
  Trigger: Component
}

/**
 * Dialog/Drawer dual-track modal. Below the md (768px) breakpoint every
 * surface renders as a bottom drawer — dialogs are hostile on phones
 * (centered card, tiny hit targets, keyboard covering the field), while a
 * drawer gets full width, full-height scrolling and native-ish dismiss
 * gestures.
 *
 * Usage mirrors the dialog primitives:
 *
 * ```vue
 * const { Modal } = useResponsiveModal()
 * ```
 * ```vue
 * <component :is="Modal.Root" :open="open" @update:open="onOpenChange">
 *   <component :is="Modal.Content">…</component>
 * </component>
 * ```
 *
 * `Content`'s own classes still apply on both tracks, so sizing written as
 * `class="flex max-h-[85vh] flex-col overflow-hidden sm:max-w-2xl"` works:
 * the dialog honours `sm:max-w-2xl`, the drawer ignores the width cap and
 * keeps its own full-width bottom layout.
 */
export function useResponsiveModal() {
  const isDesktop = useMediaQuery(MODAL_DESKTOP_QUERY)

  const Modal = computed<ModalComponents>(() =>
    isDesktop.value
      ? {
          Root: Dialog,
          Content: DialogContent,
          Header: DialogHeader,
          Title: DialogTitle,
          Description: DialogDescription,
          Footer: DialogFooter,
          Close: DialogClose,
          Trigger: DialogTrigger,
        }
      : {
          Root: Drawer,
          Content: DrawerContent,
          Header: DrawerHeader,
          Title: DrawerTitle,
          Description: DrawerDescription,
          Footer: DrawerFooter,
          Close: DrawerClose,
          Trigger: DrawerTrigger,
        },
  )

  return { isDesktop, Modal }
}
