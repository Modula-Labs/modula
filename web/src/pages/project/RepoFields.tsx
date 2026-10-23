import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Combobox } from "@/components/ui/Combobox";
import { Lock } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { relativeTime } from "@/lib/format";
import { github, rankBranches, rankRepos, type GithubRepo } from "@/lib/github";
import { useConnectGithub, useGithub } from "@/pages/settings/IntegrationsGithub";

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

/**
 * Suggestions for "New service → GitHub repo". With GitHub connected, the
 * repository and branch fields become comboboxes over what the workspace's
 * installations reach; free text (`owner/name`, a GitHub URL) always works for
 * public repositories. Not connected yet, "Connect GitHub" comes back to `returnTo`.
 */
export function useRepoSuggestions(
  workspaceId: string | undefined,
  repoInput: string,
  repo: string | null,
  branch: string,
  returnTo: string,
) {
  const { connected } = useGithub(workspaceId ?? "");
  const connect = useConnectGithub(workspaceId ?? "", returnTo);
  const ws = connected ? workspaceId : undefined;

  const repos = useSuggest(["github-repos", ws], !!ws, repoInput, (q) => github.repos(ws!, q), rankRepos);
  // Branches only for a repository one of the installations reaches.
  const [picked, setPicked] = useState<string | null>(null);
  const known = !!repo && repos.options.some((r) => r.fullName.toLowerCase() === repo.toLowerCase());
  const branchRepo = repo && (known || picked === repo) ? repo : null;
  const branches = useSuggest(
    ["github-branches", ws, branchRepo],
    !!ws && !!branchRepo,
    branch,
    (q) => github.branches(ws!, branchRepo!, q),
    rankBranches,
  );
  return {
    connected,
    repoOptions: repos.options,
    reposLoading: repos.loading,
    branchOptions: branchRepo ? branches.options : null,
    branchesLoading: branches.loading,
    setPicked,
    connect,
  };
}

/** The first page (most relevant 100) is fetched once and filtered as you type; the
 *  server is searched (debounced, from its one-minute cache) only when that page is
 *  full and has few matches. */
function useSuggest<T>(
  key: unknown[],
  enabled: boolean,
  input: string,
  fetch: (q: string) => Promise<T[]>,
  rank: (items: T[], q: string) => T[],
) {
  const base = useQuery({ queryKey: [...key, ""], queryFn: () => fetch(""), enabled, staleTime: 60_000, retry: false });
  const local = rank(base.data ?? [], input);
  const q = useDebounced(input.trim(), 200);
  const searchServer = enabled && !!q && (base.data?.length ?? 0) >= 100 && local.length < 10;
  const search = useQuery({
    queryKey: [...key, q],
    queryFn: () => fetch(q),
    enabled: searchServer,
    staleTime: 60_000,
    placeholderData: keepPreviousData,
    retry: false,
  });
  const options = searchServer && search.data ? rank(search.data, input) : local;
  return { options: options.slice(0, 50), loading: base.isFetching || (searchServer && search.isFetching) };
}

export type RepoSuggestions = ReturnType<typeof useRepoSuggestions>;

export function RepoField({
  s,
  repoInput,
  onRepoInput,
  repo,
  onPick,
}: {
  s: RepoSuggestions;
  repoInput: string;
  onRepoInput: (v: string) => void;
  /** `repoInput` parsed as `owner/name`, or null. */
  repo: string | null;
  onPick: (r: GithubRepo) => void;
}) {
  // While connected, what's typed may just be a search.
  const invalid = repoInput.trim() && !repo && (!s.connected || repoInput.includes("/"));
  const c = s.connect;
  const hint = s.connected ? (
    "Search your connected repositories, or paste owner/name of a public one."
  ) : c.loading ? (
    "Public repositories work as is."
  ) : c.mode === "connect" ? (
    <>
      Public repositories work as is.{" "}
      <button
        type="button"
        disabled={c.busy}
        onClick={() => c.connect.mutate(undefined)}
        className="font-medium text-foreground underline underline-offset-4 disabled:opacity-60"
      >
        {c.busy ? "Connecting GitHub…" : "Connect GitHub"}
      </button>{" "}
      for private ones and deploys on push.
    </>
  ) : (
    <>Public repositories work as is. {c.help}</>
  );
  const error = invalid ? "Use owner/name or a github.com URL" : (c.connect.error?.message ?? null);
  return (
    <Field label="Repository" hint={hint} error={error}>
      {(p) =>
        s.connected ? (
          <Combobox
            inputProps={p}
            mono
            autoFocus
            placeholder="Search or paste owner/name"
            value={repoInput}
            onChange={onRepoInput}
            options={s.repoOptions}
            optionKey={(r) => r.fullName}
            loading={s.reposLoading}
            empty={repoInput.trim() ? "No connected repository matches. owner/name of a public one works too." : null}
            renderOption={(r) => (
              <>
                {r.ownerAvatar ? <img src={r.ownerAvatar} alt="" className="size-5 shrink-0 rounded-full" /> : null}
                <span className="min-w-0 flex-1 truncate font-mono text-[13px]">{r.fullName}</span>
                {r.private ? <Lock aria-label="Private" className="size-3.5 shrink-0 text-muted-foreground" /> : null}
                {r.updatedAt ? <span className="shrink-0 text-xs text-muted-foreground">{relativeTime(r.updatedAt)}</span> : null}
              </>
            )}
            onPick={(r) => {
              s.setPicked(r.fullName);
              onPick(r);
            }}
          />
        ) : (
          <Input
            {...p}
            mono
            autoFocus
            required
            placeholder="owner/name or https://github.com/owner/name"
            value={repoInput}
            onChange={(e) => onRepoInput(e.target.value)}
          />
        )
      }
    </Field>
  );
}

export function BranchField({ s, branch, onBranch }: { s: RepoSuggestions; branch: string; onBranch: (v: string) => void }) {
  const options = s.branchOptions;
  return (
    <Field label="Branch">
      {(p) =>
        options ? (
          <Combobox
            inputProps={p}
            mono
            value={branch}
            onChange={onBranch}
            options={options}
            optionKey={(b) => b.name}
            loading={s.branchesLoading}
            renderOption={(b) => (
              <>
                <span className="min-w-0 flex-1 truncate font-mono text-[13px]">{b.name}</span>
                {b.protected ? <Lock aria-label="Protected" className="size-3.5 shrink-0 text-muted-foreground" /> : null}
              </>
            )}
            onPick={(b) => onBranch(b.name)}
          />
        ) : (
          <Input {...p} mono value={branch} onChange={(e) => onBranch(e.target.value)} />
        )
      }
    </Field>
  );
}
