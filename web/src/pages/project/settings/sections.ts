export const SETTINGS_SECTIONS = [
  { id: "general", label: "General" },
  { id: "usage", label: "Usage" },
  { id: "environments", label: "Environments" },
  { id: "shared-variables", label: "Shared Variables" },
  { id: "webhooks", label: "Webhooks" },
  { id: "members", label: "Members" },
  { id: "tokens", label: "Tokens" },
  { id: "integrations", label: "Integrations" },
  { id: "danger", label: "Danger" },
] as const;

export type SettingsSectionId = (typeof SETTINGS_SECTIONS)[number]["id"];

/** The section named in the URL; anything unknown shows General. */
export function parseSection(s: unknown): SettingsSectionId {
  return SETTINGS_SECTIONS.find((x) => x.id === s)?.id ?? "general";
}
