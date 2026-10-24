import { useQuery } from "@tanstack/react-query";
import { Field, Input, Select } from "@/components/ui/Input";
import { Alert, Skeleton } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import type { BucketChoice } from "@/lib/api";
import { cloudflare } from "@/lib/integrations";
import { validBucketName } from "@/lib/templates";

export type R2Mode = "new" | "existing";

/** The account's R2 buckets (live from Cloudflare). */
export function useR2Buckets(workspaceId: string | undefined, enabled = true) {
  return useQuery({
    queryKey: ["cloudflare-r2-buckets", workspaceId],
    queryFn: () => cloudflare.r2Buckets(workspaceId ?? ""),
    enabled: !!workspaceId && enabled,
    staleTime: 30_000,
    retry: false,
  });
}

/** What the fields describe, or null while incomplete or invalid. */
export function r2Choice(
  mode: R2Mode,
  bucket: string,
  existing: string,
): BucketChoice | null {
  if (mode === "existing") return existing ? { existing } : null;
  const name = bucket.trim();
  return validBucketName(name) ? { bucket: name } : null;
}

/** "New bucket" (a name) or "Link existing" (one of the account's buckets). */
export function R2ChoiceFields({
  workspaceId,
  mode,
  onMode,
  bucket,
  onBucket,
  existing,
  onExisting,
}: {
  workspaceId: string | undefined;
  mode: R2Mode;
  onMode: (m: R2Mode) => void;
  bucket: string;
  onBucket: (b: string) => void;
  existing: string;
  onExisting: (b: string) => void;
}) {
  const buckets = useR2Buckets(workspaceId, mode === "existing");
  const trimmed = bucket.trim();
  return (
    <div className="flex flex-col gap-3">
      <Segmented
        label="Bucket"
        value={mode}
        onChange={onMode}
        options={[
          { value: "new", label: "New bucket" },
          { value: "existing", label: "Link existing" },
        ]}
      />
      {mode === "new" ? (
        <Field
          label="Bucket name"
          hint="Created in your Cloudflare account. Lowercase letters, digits and hyphens."
          error={
            trimmed && !validBucketName(trimmed)
              ? "3–63 lowercase letters, digits and hyphens, no hyphen at either end"
              : null
          }
        >
          {(p) => (
            <Input
              {...p}
              mono
              value={bucket}
              onChange={(e) => onBucket(e.target.value)}
            />
          )}
        </Field>
      ) : buckets.isPending ? (
        <Skeleton className="h-9" />
      ) : buckets.error ? (
        <Alert>{buckets.error.message}</Alert>
      ) : buckets.data.buckets.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          The Cloudflare account has no buckets yet.
        </p>
      ) : (
        <Field
          label="Bucket"
          hint="Gets its own credentials, limited to this bucket. Its data stays where it is."
        >
          {(p) => (
            <Select
              {...p}
              value={existing}
              onChange={(e) => onExisting(e.target.value)}
            >
              <option value="">Choose a bucket…</option>
              {buckets.data.buckets.map((b) => (
                <option key={b.name} value={b.name}>
                  {b.name}
                  {b.links.length
                    ? ` (used by ${b.links.map((l) => `${l.serviceName}/${l.environmentName}`).join(", ")})`
                    : ""}
                </option>
              ))}
            </Select>
          )}
        </Field>
      )}
    </div>
  );
}
