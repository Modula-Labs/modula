import { useNavigate } from "@tanstack/react-router";
import { Button } from "@/components/ui/Button";
import { SettingsSection } from "./SettingsParts";

/** Members, tokens and integrations are workspace-wide in Modula; point to where they live. */
export function WorkspaceLinkSection({ title, description, to, label }: { title: string; description: string; to: "/settings" | "/account/security"; label: string }) {
  const navigate = useNavigate();
  return (
    <SettingsSection title={title} description={description}>
      <div>
        <Button onClick={() => void navigate({ to })}>{label}</Button>
      </div>
    </SettingsSection>
  );
}
