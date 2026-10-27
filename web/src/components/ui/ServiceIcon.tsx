import { clsx } from "clsx";
import { Box, Bucket } from "@/components/ui/icons";
import { MONO_STACK_ICONS, resolveStackIcon } from "@/lib/stack";

const urls = import.meta.glob<string>("../../assets/stack/*.svg", { query: "?url", import: "default", eager: true });
const byId: Record<string, string> = {};
for (const [path, url] of Object.entries(urls)) {
  const id = path.split("/").pop()?.replace(/\.svg$/, "");
  if (id) byId[id] = url;
}
// Valkey is a Redis fork with no icon in the open sets we bundle.
byId.valkey = byId.redis ?? "";

const SIZES = { sm: "size-6 rounded-md", md: "size-8 rounded-lg", lg: "size-10 rounded-lg" } as const;
const GLYPH = { sm: "size-5", md: "size-7", lg: "size-9" } as const;
// Placeholder glyphs (bucket, unknown) are smaller line icons.
const PLACEHOLDER = { sm: "size-3.5", md: "size-5", lg: "size-6" } as const;

/**
 * Brand mark for a service, on a transparent tile (no fill, no border) so the logo fills the footprint.
 * Colour logos have near-black brand fills lightened in the SVGs; single-colour logos are drawn as a CSS
 * mask filled with `currentColor`, so they flip correctly between light and dark. Only the generic and
 * bucket placeholders get a subtle accent tile.
 */
export function ServiceIcon({
  stack,
  image,
  repo,
  size = "md",
  className,
}: {
  stack?: string | null;
  image?: string | null;
  repo?: string | null;
  size?: keyof typeof SIZES;
  className?: string;
}) {
  const id = resolveStackIcon({ stack, image, repo });
  const url = byId[id];
  const mono = MONO_STACK_ICONS.has(id);
  return (
    <span
      title={id === "generic" ? undefined : id}
      className={clsx(
        "inline-flex shrink-0 items-center justify-center text-foreground",
        SIZES[size],
        !url || id === "bucket" ? "border border-border bg-accent" : "",
        className,
      )}
    >
      {id === "bucket" ? (
        <Bucket aria-hidden className={clsx(PLACEHOLDER[size], "text-muted-foreground")} />
      ) : !url ? (
        <Box aria-hidden className={clsx(PLACEHOLDER[size], "text-muted-foreground")} />
      ) : mono ? (
        <span
          aria-hidden
          className={clsx(GLYPH[size], "bg-current")}
          style={{
            maskImage: `url(${url})`,
            WebkitMaskImage: `url(${url})`,
            maskSize: "contain",
            WebkitMaskSize: "contain",
            maskRepeat: "no-repeat",
            WebkitMaskRepeat: "no-repeat",
            maskPosition: "center",
            WebkitMaskPosition: "center",
          }}
        />
      ) : (
        <img src={url} alt="" aria-hidden draggable={false} className={GLYPH[size]} />
      )}
    </span>
  );
}
