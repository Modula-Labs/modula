import type { NodeInfo } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";

export const CPU_STEP = 250; // millicores
export const MEM_STEP = 256; // MB
export const FALLBACK_CPUS = 8;
export const FALLBACK_MEMORY_MB = 16 * 1024;

const trim = (n: number, digits = 2) => String(Number(n.toFixed(digits)));

/** The server's capacity, in the units the config stores. */
export function capacity(node: NodeInfo | undefined | null): { cpuMillis: number; memoryMb: number } {
  const cpus = node?.info.cpus;
  const bytes = node?.info.memoryBytes;
  return {
    cpuMillis: (cpus && cpus > 0 ? cpus : FALLBACK_CPUS) * 1000,
    memoryMb: bytes && bytes > 0 ? Math.floor(bytes / 1024 ** 2) : FALLBACK_MEMORY_MB,
  };
}

/** Number of slider stops; the last one means "no limit". */
export const stepsFor = (maxValue: number, step: number) => Math.max(2, Math.floor(maxValue / step));

/** Slider position for a stored limit (empty = no limit = all the way right). */
export function toSlider(value: string, maxValue: number, step: number): number {
  const steps = stepsFor(maxValue, step);
  const n = Number(value);
  if (!value.trim() || !Number.isFinite(n) || n <= 0) return steps;
  return Math.min(steps - 1, Math.max(1, Math.round(n / step)));
}

/** The form string for a slider position: empty (no limit) at the right end. */
export function fromSlider(pos: number, maxValue: number, step: number): string {
  const steps = stepsFor(maxValue, step);
  return pos >= steps ? "" : String(Math.max(1, Math.round(pos)) * step);
}

export function formatCpu(millis: string | number | undefined, noLimit = "No limit"): string {
  const n = Number(millis);
  if (millis === undefined || millis === "" || !n) return noLimit;
  return `${trim(n / 1000)} vCPU`;
}

export function formatMemory(mb: string | number | undefined, noLimit = "No limit"): string {
  const n = Number(mb);
  if (mb === undefined || mb === "" || !n) return noLimit;
  return n >= 1024 ? `${trim(n / 1024)} GB` : `${n} MB`;
}

/** "4 vCPU · 8 GB" */
export function serverSize(node: NodeInfo): string {
  const { cpuMillis, memoryMb } = capacity(node);
  return `${formatCpu(cpuMillis)} · ${formatMemory(memoryMb)}`;
}

/** "🇩🇪 Falkenstein · modula-main (4 vCPU · 8 GB)" */
export function serverLabel(node: NodeInfo): string {
  const flag = flagEmoji(node.location?.countryCode);
  const where = node.location?.label;
  const head = [flag, where].filter(Boolean).join(" ");
  return `${head ? `${head} · ` : ""}${node.name} (${serverSize(node)})`;
}
