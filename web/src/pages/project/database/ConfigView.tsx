import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Database, Eye, EyeOff, Plug, Puzzle, Search } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Kbd } from "@/components/ui/misc";
import { api, type DbCredentials, type DbExtension, type Instance } from "@/lib/api";
import { Card, QueryBoundary, dbQueryOpts } from "./shared";

interface Section {
  id: string;
  title: string;
  keywords: string;
  icon: ReactNode;
  body: ReactNode;
}

export function ConfigView({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const [filter, setFilter] = useState("");
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (e.key === "/" && !e.metaKey && !e.ctrlKey && t && !/INPUT|TEXTAREA|SELECT/.test(t.tagName) && !t.isContentEditable) {
        e.preventDefault();
        input.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const image = instance.config.source.type === "image" ? instance.config.source.image : null;
  const all: Section[] = [
    {
      id: "connection",
      title: "Connection",
      keywords: "connection username password credentials regenerate host port",
      icon: <Plug />,
      body: <Connection instance={instance} canEdit={canEdit} />,
    },
    {
      id: "extensions",
      title: "Extensions",
      keywords: "extensions install uninstall postgis pgvector pg_stat_statements",
      icon: <Puzzle />,
      body: <Extensions instance={instance} canEdit={canEdit} />,
    },
    {
      id: "version",
      title: "Version",
      keywords: "version image postgres",
      icon: <Database />,
      body: (
        <Card className="flex h-10 items-center px-3 font-mono text-[13px]">{image ?? "Unknown image"}</Card>
      ),
    },
  ];
  const f = filter.trim().toLowerCase();
  const sections = f ? all.filter((s) => s.keywords.includes(f) || s.title.toLowerCase().includes(f)) : all;

  return (
    <div className="flex flex-col gap-6">
      <div className="relative">
        <Search className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground" />
        <Input
          ref={input}
          aria-label="Filter settings"
          placeholder="Filter settings…"
          className="pr-10 pl-9"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              setFilter("");
              e.currentTarget.blur();
            }
          }}
        />
        {!filter ? (
          <div className="pointer-events-none absolute top-1/2 right-3 -translate-y-1/2">
            <Kbd>/</Kbd>
          </div>
        ) : null}
      </div>
      <div className="grid grid-cols-[1fr_160px] gap-8">
        <div className="flex min-w-0 flex-col">
          {sections.map((s, i) => (
            <section key={s.id} id={`db-config-${s.id}`} className="relative flex gap-4 pb-8 last:pb-0">
              <div className="flex flex-col items-center">
                <div className="flex size-8 shrink-0 items-center justify-center rounded-full border border-border bg-card text-foreground [&_svg]:size-4">
                  {s.icon}
                </div>
                {i < sections.length - 1 ? <div className="mt-1 w-px flex-1 bg-border" /> : null}
              </div>
              <div className="min-w-0 flex-1">
                <h3 className="mb-3 flex h-8 items-center text-base font-bold">{s.title}</h3>
                {s.body}
              </div>
            </section>
          ))}
          {sections.length === 0 ? <p className="text-sm text-muted-foreground">No settings match “{filter}”.</p> : null}
        </div>
        <nav aria-label="On this page" className="sticky top-0 self-start text-sm">
          <ul className="flex flex-col gap-1 border-l border-border">
            {sections.map((s) => (
              <li key={s.id}>
                <button
                  type="button"
                  className="-ml-px w-full border-l border-transparent px-3 py-1 text-left text-muted-foreground hover:border-foreground hover:text-foreground"
                  onClick={() => document.getElementById(`db-config-${s.id}`)?.scrollIntoView({ behavior: "smooth", block: "start" })}
                >
                  {s.title}
                </button>
              </li>
            ))}
          </ul>
        </nav>
      </div>
    </div>
  );
}

function ValueField({ label, value, secret }: { label: string; value: string; secret?: boolean }) {
  const [shown, setShown] = useState(false);
  return (
    <div className="flex flex-col gap-1.5">
      <div className="text-sm font-bold">{label}</div>
      <div className="flex h-8 items-center gap-1 rounded-lg border border-input pr-0.5 pl-2.5 dark:bg-input/30">
        <span className="min-w-0 flex-1 truncate font-mono text-[13px]">{secret && !shown ? "•".repeat(Math.min(24, Math.max(value.length, 8))) : value}</span>
        {secret ? (
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={shown ? `Hide ${label}` : `Reveal ${label}`}
            onClick={() => setShown((s) => !s)}
            className="text-muted-foreground"
          >
            {shown ? <EyeOff /> : <Eye />}
          </Button>
        ) : null}
        <CopyButton value={value} label={`Copy ${label}`} />
      </div>
    </div>
  );
}

function Connection({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["db-credentials", instance.id];
  // Every read of the credentials is audited: fetch on request, so merely
  // opening the tab doesn't record one.
  const [reveal, setReveal] = useState(false);
  const q = useQuery({ queryKey: key, queryFn: () => api.dbCredentials(instance.id), ...dbQueryOpts, enabled: reveal });
  const [confirm, setConfirm] = useState(false);
  const regen = useMutation({
    mutationFn: () => api.dbRegeneratePassword(instance.id),
    onSuccess: (c: DbCredentials) => {
      qc.setQueryData(key, c);
      void qc.invalidateQueries({ queryKey: ["variables", instance.id] });
      setConfirm(false);
    },
  });
  if (!reveal) {
    return (
      <Card className="flex items-center justify-between gap-4 p-4">
        <p className="text-sm text-muted-foreground">Credentials give full access to this database. Viewing them is recorded in the audit log.</p>
        <Button variant="outline" onClick={() => setReveal(true)}>
          Show credentials
        </Button>
      </Card>
    );
  }
  return (
    <QueryBoundary q={q}>
      {(c) => (
        <div className="flex flex-col gap-4">
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <ValueField label="Username" value={c.username} />
            <ValueField label="Password" value={c.password} secret />
          </div>
          <Card className="flex items-center justify-between gap-4 p-4">
            <div>
              <div className="text-sm font-bold">Regenerate password</div>
              <p className="text-sm text-muted-foreground">Breaks existing connections until they use the new password.</p>
            </div>
            {canEdit ? (
              <Button variant="danger-outline" onClick={() => setConfirm(true)}>
                Regenerate
              </Button>
            ) : null}
          </Card>
          <Modal
            open={confirm}
            onOpenChange={(o) => {
              setConfirm(o);
              if (!o) regen.reset();
            }}
            title="Regenerate password"
            description="Every client using the current password loses access until it is updated. Services that reference DATABASE_URL pick up the new value on their next deploy."
            footer={
              <>
                <Button variant="ghost" onClick={() => setConfirm(false)}>
                  Cancel
                </Button>
                <Button variant="danger" loading={regen.isPending} onClick={() => regen.mutate()}>
                  Regenerate password
                </Button>
              </>
            }
          >
            {regen.error ? <Alert>{regen.error.message}</Alert> : null}
          </Modal>
        </div>
      )}
    </QueryBoundary>
  );
}

function Extensions({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["db-extensions", instance.id], queryFn: () => api.dbExtensions(instance.id), ...dbQueryOpts });
  const [search, setSearch] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const toggle = useMutation({
    mutationFn: (v: { name: string; action: "install" | "uninstall" }) => {
      setBusy(v.name);
      return api.dbSetExtension(instance.id, v.name, v.action);
    },
    onSettled: () => {
      setBusy(null);
      void qc.invalidateQueries({ queryKey: ["db-extensions", instance.id] });
      void qc.invalidateQueries({ queryKey: ["db-stats", instance.id] });
    },
  });
  const row = (e: DbExtension, action: "install" | "uninstall") => (
    <li key={e.name} className="flex items-center gap-3 border-t border-border px-3 py-2.5 first:border-t-0 hover:bg-muted/50">
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <span className="font-mono text-[13px] font-bold">{e.name}</span>
          <span className="text-[13px] text-muted-foreground">{e.version}</span>
        </div>
        {e.comment ? <div className="truncate text-[13px] text-muted-foreground">{e.comment}</div> : null}
      </div>
      {canEdit ? (
        <Button variant="outline" loading={busy === e.name} disabled={busy !== null} onClick={() => toggle.mutate({ name: e.name, action })}>
          {action === "install" ? "Install" : "Uninstall"}
        </Button>
      ) : null}
    </li>
  );
  return (
    <QueryBoundary q={q}>
      {(d) => {
        const available = d.available.filter((e) => e.name.toLowerCase().includes(search.toLowerCase()) || e.comment.toLowerCase().includes(search.toLowerCase()));
        return (
          <div className="flex flex-col gap-5">
            {toggle.error ? <Alert>{toggle.error.message}</Alert> : null}
            <div className="flex flex-col gap-2">
              <div className="text-sm font-bold">
                {d.installed.length} {d.installed.length === 1 ? "Extension" : "Extensions"} installed
              </div>
              {d.installed.length > 0 ? <Card><ul>{d.installed.map((e) => row(e, "uninstall"))}</ul></Card> : null}
            </div>
            <div className="flex flex-col gap-2">
              <div className="flex items-center justify-between gap-3">
                <div className="text-sm font-bold">
                  {d.available.length} {d.available.length === 1 ? "Extension" : "Extensions"} available
                </div>
                <Input className="h-8 max-w-56" aria-label="Search extensions" placeholder="Search extensions…" value={search} onChange={(e) => setSearch(e.target.value)} />
              </div>
              <Card>
                <ul className="max-h-80 overflow-auto">{available.map((e) => row(e, "install"))}</ul>
                {available.length === 0 ? <p className="px-3 py-6 text-center text-sm text-muted-foreground">No extensions match.</p> : null}
              </Card>
            </div>
          </div>
        );
      }}
    </QueryBoundary>
  );
}
