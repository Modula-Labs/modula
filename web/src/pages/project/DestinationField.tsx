import { Field, Select } from "@/components/ui/Input";
import type { BackupDestinations } from "@/lib/api";
import type { DestinationGroup } from "@/lib/backup";

/** What to tell the user about where a destination keeps its data. */
export function destinationNote(value: string, dests: BackupDestinations): string | null {
  if (value === "local") {
    return "Kept on that server only: if the server is lost, so are these backups. Restores need the database to run there.";
  }
  const bucket = value.startsWith("bucket:") ? dests.buckets.find((b) => `bucket:${b.instanceId}` === value) : undefined;
  return bucket?.template === "bucket" ? "Uploaded by the database's server over the private network." : null;
}

/** The Storage select: storage targets, bucket services and the server's disk. */
export function DestinationField({
  groups,
  dests,
  value,
  onChange,
  disabled,
}: {
  groups: DestinationGroup[];
  dests: BackupDestinations;
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
}) {
  return (
    <Field label="Storage" hint={destinationNote(value, dests) ?? undefined}>
      {(p) => (
        <Select {...p} disabled={disabled} value={value} onChange={(e) => onChange(e.target.value)}>
          {groups.map((g) => (
            <optgroup key={g.label} label={g.label}>
              {g.options.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.hint ? `${o.label} · ${o.hint}` : o.label}
                </option>
              ))}
            </optgroup>
          ))}
        </Select>
      )}
    </Field>
  );
}
