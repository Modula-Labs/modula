import { clsx } from "clsx";
import { BrandDocker, BrandGithub, IconUpload } from "@/components/ui/icons";
import type { Deployment } from "@/lib/api";

/** Source icon in a rounded square; the commit author's avatar overlaps its corner. */
export function SourceAvatar({ d, size = 32 }: { d: Pick<Deployment, "sourceKind" | "commitSha" | "commitAuthorAvatar" | "commitAuthor">; size?: 32 | 40 }) {
  const kind = d.sourceKind ?? (d.commitSha ? "github" : "image");
  const Icon = kind === "github" ? BrandGithub : kind === "upload" ? IconUpload : BrandDocker;
  return (
    <span
      aria-hidden
      className={clsx(
        "relative inline-flex shrink-0 items-center justify-center rounded-lg bg-accent text-muted-foreground [&_svg]:size-4",
        size === 32 ? "size-8" : "size-10 [&_svg]:size-5",
      )}
    >
      <Icon />
      {d.commitAuthorAvatar ? (
        <img
          src={d.commitAuthorAvatar}
          alt=""
          loading="lazy"
          referrerPolicy="no-referrer"
          className="absolute -right-1 -bottom-1 size-4 rounded-full border-2 border-card bg-muted object-cover"
        />
      ) : null}
    </span>
  );
}
