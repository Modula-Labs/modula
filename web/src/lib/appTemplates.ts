import type { AppTemplate, AppTemplateService, Template } from "./api";
import { imageBaseName } from "./stack";

/** Apps matching every word of `query` (name, description, category or a service), in gallery order. */
export function filterAppTemplates(apps: AppTemplate[], query: string): AppTemplate[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return apps;
  return apps.filter((a) => {
    const text = [a.id, a.name, a.description, a.category, ...a.services.map((s) => s.role)].join(" ").toLowerCase();
    return words.every((w) => text.includes(w));
  });
}

/** `1 service`, `3 services`. */
export const serviceCount = (app: AppTemplate) =>
  `${app.services.length} ${app.services.length === 1 ? "service" : "services"}`;

/** Up to two initials for the app's tile: `Uptime Kuma` → `UK`, `n8n` → `N`. */
export function appMonogram(name: string): string {
  const words = name.split(/[\s-]+/).filter((w) => /[a-z0-9]/i.test(w));
  return words
    .slice(0, 2)
    .map((w) => w.match(/[a-z0-9]/i)?.[0] ?? "")
    .join("")
    .toUpperCase();
}

/** What a service of the app is, for the detail view: `PostgreSQL database` or the image's name and tag. */
export function describeAppService(s: AppTemplateService, databases: Template[] = []): string {
  if (s.template) return `${databases.find((t) => t.id === s.template)?.name ?? s.template} database`;
  const tag = s.image.split("@")[0]?.split("/").pop()?.split(":")[1];
  return tag ? `${imageBaseName(s.image)} ${tag}` : imageBaseName(s.image);
}

