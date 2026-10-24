import {
  keepPreviousData,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Combobox } from "@/components/ui/Combobox";
import {
  BrandGithub,
  Check,
  ChevronDown,
  Lock,
  Plus,
  Refresh,
} from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { relativeTime } from "@/lib/format";
import {
  effectiveAccount,
  filterReposByAccount,
  github,
  rankBranches,
  rankRepos,
  type GithubInstallation,
  type GithubRepo,
} from "@/lib/github";
import {
  useConnectGithub,
  useGithub,
} from "@/pages/settings/IntegrationsGithub";

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
  const qc = useQueryClient();
  const { connected, installations } = useGithub(workspaceId ?? "");
  const connect = useConnectGithub(workspaceId ?? "", returnTo);
  const ws = connected ? workspaceId : undefined;
  const accounts = installations.data ?? [];

  // Same query as the list's first page (shared by the cache): what picks the default account.
  const firstPage = useQuery({
    queryKey: ["github-repos", ws, ""],
    queryFn: () => github.repos(ws!, ""),
    enabled: !!ws,
    staleTime: 60_000,
    retry: false,
  });
  const [selected, setSelected] = useState<string | null>(null);
  const account = connected
    ? effectiveAccount(selected, accounts, firstPage.data ?? [])
    : null;

  const repos = useSuggest(
    ["github-repos", ws],
    !!ws,
    repoInput,
    (q) => github.repos(ws!, q),
    rankRepos,
    (items) => filterReposByAccount(items, account),
  );
  // Branches only for a repository one of the installations reaches (any account: a pasted
  // owner/name of another installed account works too).
  const [picked, setPicked] = useState<string | null>(null);
  const known =
    !!repo &&
    (firstPage.data ?? [])
      .concat(repos.options)
      .some((r) => r.fullName.toLowerCase() === repo.toLowerCase());
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
    accounts,
    account,
    selectAccount: setSelected,
    /** Refetch repositories (after changing which ones GitHub shares with Modula). */
    refresh: () => {
      void qc.invalidateQueries({ queryKey: ["github-repos"] });
      void qc.invalidateQueries({ queryKey: ["github-installations"] });
    },
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
  narrow: (items: T[]) => T[] = (x) => x,
) {
  const base = useQuery({
    queryKey: [...key, ""],
    queryFn: () => fetch(""),
    enabled,
    staleTime: 60_000,
    retry: false,
  });
  const local = narrow(rank(base.data ?? [], input));
  const q = useDebounced(input.trim(), 200);
  const searchServer =
    enabled && !!q && (base.data?.length ?? 0) >= 100 && local.length < 10;
  const search = useQuery({
    queryKey: [...key, q],
    queryFn: () => fetch(q),
    enabled: searchServer,
    staleTime: 60_000,
    placeholderData: keepPreviousData,
    retry: false,
  });
  const options =
    searchServer && search.data ? narrow(rank(search.data, input)) : local;
  return {
    options: options.slice(0, 50),
    loading: base.isFetching || (searchServer && search.isFetching),
  };
}

export type RepoSuggestions = ReturnType<typeof useRepoSuggestions>;

export function RepoField({
  s,
  repoInput,
  onRepoInput,
  repo,
  onPick,
  autoFocus = true,
  disabled,
}: {
  s: RepoSuggestions;
  autoFocus?: boolean;
  disabled?: boolean;
  repoInput: string;
  onRepoInput: (v: string) => void;
  /** `repoInput` parsed as `owner/name`, or null. */
  repo: string | null;
  onPick: (r: GithubRepo) => void;
}) {
  // While connected, what's typed may just be a search.
  const invalid =
    repoInput.trim() && !repo && (!s.connected || repoInput.includes("/"));
  const c = s.connect;
  const configureUrl = s.accounts.find(
    (i) => i.accountLogin === s.account,
  )?.configureUrl;
  const hint = s.connected ? (
    <span className="flex flex-wrap items-center gap-x-1.5">
      <span>Missing a repository?</span>
      {configureUrl ? (
        <a
          href={configureUrl}
          target="_blank"
          rel="noreferrer"
          className="font-normal text-foreground underline underline-offset-4"
        >
          Configure access on GitHub
        </a>
      ) : null}
      <Button
        type="button"
        variant="ghost"
        size="icon-xs"
        onClick={s.refresh}
        disabled={s.reposLoading}
        aria-label="Refresh repositories"
        title="Refresh repositories"
        className="text-muted-foreground"
      >
        <Refresh className={s.reposLoading ? "animate-spin" : undefined} />
      </Button>
    </span>
  ) : (
    "Public repositories work as is: paste owner/name or a GitHub URL."
  );
  const error = invalid
    ? "Use owner/name or a github.com URL"
    : (c.connect.error?.message ?? null);
  return (
    <>
      {s.connected ? (
        <AccountPicker s={s} disabled={disabled} />
      ) : (
        <ConnectPanel s={s} disabled={disabled} />
      )}
      <Field label="Repository" hint={hint} error={error}>
        {(p) =>
          s.connected ? (
            <Combobox
              inputProps={p}
              mono
              autoFocus={autoFocus}
              disabled={disabled}
              placeholder="Search or paste owner/name"
              value={repoInput}
              onChange={onRepoInput}
              options={s.repoOptions}
              optionKey={(r) => r.fullName}
              loading={s.reposLoading}
              empty={
                repoInput.trim()
                  ? "No repository of this account matches. owner/name of a public one works too."
                  : null
              }
              renderOption={(r) => (
                <>
                  {r.ownerAvatar ? (
                    <img
                      src={r.ownerAvatar}
                      alt=""
                      className="size-5 shrink-0 rounded-full"
                    />
                  ) : null}
                  <span className="min-w-0 flex-1 truncate font-mono text-[13px]">
                    {r.fullName}
                  </span>
                  {r.private ? (
                    <Lock
                      aria-label="Private"
                      className="size-3.5 shrink-0 text-muted-foreground"
                    />
                  ) : null}
                  {r.updatedAt ? (
                    <span className="shrink-0 text-xs text-muted-foreground">
                      {relativeTime(r.updatedAt)}
                    </span>
                  ) : null}
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
              autoFocus={autoFocus}
              disabled={disabled}
              required
              placeholder="owner/name or https://github.com/owner/name"
              value={repoInput}
              onChange={(e) => onRepoInput(e.target.value)}
            />
          )
        }
      </Field>
    </>
  );
}

function AccountPicker({
  s,
  disabled,
}: {
  s: RepoSuggestions;
  disabled?: boolean;
}) {
  const c = s.connect;
  const current = s.accounts.find((i) => i.accountLogin === s.account);
  return (
    <Field label="GitHub account">
      {(p) => (
        <Menu
          align="start"
          trigger={
            <Button
              type="button"
              variant="outline"
              id={p.id}
              disabled={disabled}
              className="w-full justify-start gap-2 px-2.5"
            >
              {current ? (
                <AccountLabel i={current} />
              ) : (
                <span className="text-muted-foreground">Choose an account</span>
              )}
              <ChevronDown className="ml-auto size-4 shrink-0 text-muted-foreground" />
            </Button>
          }
        >
          {s.accounts.map((i) => (
            <MenuItem
              key={i.id}
              onSelect={() => s.selectAccount(i.accountLogin)}
              icon={
                i.accountLogin === s.account ? (
                  <Check />
                ) : (
                  <span className="size-4" />
                )
              }
            >
              <AccountLabel i={i} />
            </MenuItem>
          ))}
          <MenuSeparator />
          <MenuItem
            icon={<Plus />}
            disabled={c.mode !== "connect" || c.busy}
            title={c.help ?? undefined}
            onSelect={() => c.connect.mutate(undefined)}
          >
            Add GitHub account or organization
          </MenuItem>
        </Menu>
      )}
    </Field>
  );
}

function AccountLabel({ i }: { i: GithubInstallation }) {
  return (
    <>
      <img
        src={i.avatarUrl}
        alt=""
        className="size-5 shrink-0 rounded-full border border-border"
      />
      <span className="min-w-0 truncate">{i.accountLogin}</span>
      <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
        {i.accountType === "Organization" ? "Org" : "User"}
      </span>
    </>
  );
}

/** No GitHub installation yet: say what connecting does, with the one-click flow. */
function ConnectPanel({
  s,
  disabled,
}: {
  s: RepoSuggestions;
  disabled?: boolean;
}) {
  const c = s.connect;
  if (c.loading) return null;
  return (
    <div className="flex flex-col gap-2 rounded-xl border border-border bg-muted/40 p-4">
      <div className="flex items-center gap-2 text-sm font-bold">
        <BrandGithub className="size-4 text-muted-foreground" />
        Connect GitHub
      </div>
      <p className="text-sm text-muted-foreground">
        Pick your personal account or an organization on GitHub, then choose
        which repositories Modula can see. Needed for private repositories and
        deploys on push.
      </p>
      {c.mode === "connect" ? (
        <Button
          type="button"
          variant="secondary"
          className="w-fit"
          icon={<BrandGithub />}
          loading={c.busy}
          disabled={disabled}
          onClick={() => c.connect.mutate(undefined)}
        >
          Connect GitHub
        </Button>
      ) : (
        <p className="text-sm">{c.help}</p>
      )}
    </div>
  );
}

export function BranchField({
  s,
  branch,
  onBranch,
  disabled,
}: {
  s: RepoSuggestions;
  branch: string;
  onBranch: (v: string) => void;
  disabled?: boolean;
}) {
  const options = s.branchOptions;
  return (
    <Field label="Branch">
      {(p) =>
        options ? (
          <Combobox
            inputProps={p}
            mono
            disabled={disabled}
            value={branch}
            onChange={onBranch}
            options={options}
            optionKey={(b) => b.name}
            loading={s.branchesLoading}
            renderOption={(b) => (
              <>
                <span className="min-w-0 flex-1 truncate font-mono text-[13px]">
                  {b.name}
                </span>
                {b.protected ? (
                  <Lock
                    aria-label="Protected"
                    className="size-3.5 shrink-0 text-muted-foreground"
                  />
                ) : null}
              </>
            )}
            onPick={(b) => onBranch(b.name)}
          />
        ) : (
          <Input
            {...p}
            mono
            disabled={disabled}
            value={branch}
            onChange={(e) => onBranch(e.target.value)}
          />
        )
      }
    </Field>
  );
}
