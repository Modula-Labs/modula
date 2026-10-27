import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Checkbox } from "@/components/ui/Controls";
import { Plug } from "@/components/ui/icons";
import { Alert, Badge } from "@/components/ui/misc";
import { cloudflare, sslModeLabel } from "@/lib/integrations";

/** Shown under a custom domain whose zone is on the workspace's Cloudflare:
 *  DNS is managed automatically, with a "DNS only" / "Proxied" switch. */
export function CloudflareIngress({
  domainId,
  canEdit,
}: {
  domainId: string;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const key = ["domain-ingress", domainId];
  const ingress = useQuery({
    queryKey: key,
    queryFn: () => cloudflare.ingress(domainId),
    enabled: canEdit,
    staleTime: 5 * 60_000,
    retry: false,
  });
  const set = useMutation({
    mutationFn: (proxied: boolean) => cloudflare.setIngress(domainId, proxied),
    onSuccess: (i) => qc.setQueryData(key, i),
  });
  const i = ingress.data;
  if (!i?.managed) return null;
  return (
    <div className="flex flex-col gap-2 border-t border-border px-3 py-2.5">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <Plug className="size-4 text-muted-foreground" />
        <span>
          DNS managed automatically with Cloudflare{" "}
          <span className="text-muted-foreground">({i.zone})</span>
        </span>
        <Checkbox
          className="ml-auto"
          checked={i.proxied}
          disabled={set.isPending}
          onCheckedChange={(on) => set.mutate(on)}
          label="Proxied (orange cloud)"
        />
        {i.proxied && i.sslMode ? (
          <Badge tone={i.sslMode === "strict" ? "neutral" : "warn"}>
            SSL: {sslModeLabel(i.sslMode)}
          </Badge>
        ) : null}
      </div>
      {i.proxied ? (
        <p className="text-[13px] text-muted-foreground">
          Traffic goes through Cloudflare's CDN and DDoS protection. The zone's
          SSL/TLS mode must be Full (strict). Your servers still accept direct
          traffic for now.
        </p>
      ) : null}
      {i.sslWarning ? <Alert tone="warn">{i.sslWarning}</Alert> : null}
      {i.note ? <Alert tone="warn">{i.note}</Alert> : null}
      {set.error ? <Alert>{set.error.message}</Alert> : null}
    </div>
  );
}
