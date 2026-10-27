// A tiny toast store: `toast("…")` from anywhere, <Toaster /> renders them.
import { useSyncExternalStore } from "react";

export interface Toast {
  id: number;
  message: string;
  tone: "info" | "error";
}

let toasts: Toast[] = [];
let next = 1;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());

export function dismissToast(id: number) {
  toasts = toasts.filter((t) => t.id !== id);
  emit();
}

export function toast(message: string, tone: Toast["tone"] = "info", ms = 5000) {
  // The same message twice in a row (a re-render loop, a retried request) shows once.
  if (toasts.some((t) => t.message === message)) return;
  const id = next++;
  toasts = [...toasts.slice(-3), { id, message, tone }];
  emit();
  window.setTimeout(() => dismissToast(id), ms);
}

export const toastError = (e: unknown, fallback = "Something went wrong.") =>
  toast(e instanceof Error && e.message ? e.message : fallback, "error");

export function useToasts(): Toast[] {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => toasts,
  );
}
