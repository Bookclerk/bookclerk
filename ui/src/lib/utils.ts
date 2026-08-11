import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * Merges Tailwind class names with conflict resolution via `tailwind-merge`.
 *
 * @param inputs - Class values accepted by `clsx`.
 * @returns Merged class string.
 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * Shared content column for SPA headers and mains.
 *
 * Grows past `max-w-6xl` on large / ultrawide viewports so shelves and tables
 * are not trapped in a narrow center gutter.
 */
export const pageWidthClass =
  "mx-auto w-full max-w-6xl lg:max-w-7xl xl:max-w-[85rem] 2xl:max-w-[96rem]";

/**
 * Title detail dialog width classes — wider than form modals for commerce + reviews.
 */
export const titleDetailDialogClass =
  "w-full max-w-3xl lg:max-w-4xl xl:max-w-5xl";
