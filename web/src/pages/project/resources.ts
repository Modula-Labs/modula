import type { NodeInfo } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";

export const CPU_STEP = 250; // millicores
export const MEM_STEP = 256; // MB
export const FALLBACK_CPUS = 8;
export const FALLBACK_MEMORY_MB = 16 * 1024;
/** What the server applies when a service sets no limit (deploy/desired.rs). */
export const DEFAULT_CPU_MILLIS = 1000;
export const DEFAULT_MEMORY_MB = 512;

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

/** Number of slider stops (`step` each); the last one is the whole server. */
export const stepsFor = (maxValue: number, step: number) => Math.max(1, Math.floor(maxValue / step));

/** Slider position for a stored limit (empty = the default, `fallback`). */
export function toSlider(value: string, maxValue: number, step: number, fallback = step): number {
  const steps = stepsFor(maxValue, step);
  const n = Number(value);
  const v = !value.trim() || !Number.isFinite(n) || n <= 0 ? fallback : n;
  return Math.min(steps, Math.max(1, Math.round(v / step)));
}

/** The form string for a slider position: always an explicit limit. */
export function fromSlider(pos: number, maxValue: number, step: number): string {
  const steps = stepsFor(maxValue, step);
  return String(Math.min(steps, Math.max(1, Math.round(pos))) * step);
}

const DEFAULT_CPU_LABEL = `${trim(DEFAULT_CPU_MILLIS / 1000)} vCPU (default)`;
const DEFAULT_MEMORY_LABEL = `${DEFAULT_MEMORY_MB} MB (default)`;

export function formatCpu(millis: string | number | undefined, unset = DEFAULT_CPU_LABEL): string {
  const n = Number(millis);
  if (millis === undefined || millis === "" || !n) return unset;
  return `${trim(n / 1000)} vCPU`;
}

export function formatMemory(mb: string | number | undefined, unset = DEFAULT_MEMORY_LABEL): string {
  const n = Number(mb);
  if (mb === undefined || mb === "" || !n) return unset;
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
