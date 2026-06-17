import type { ClassValue } from "clsx"
import { clsx } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

export function getErrorMessage(e: unknown, fallback = "Unknown error"): string {
  return (e as any)?.response?.data?.error?.message ?? (e as Error)?.message ?? fallback
}
