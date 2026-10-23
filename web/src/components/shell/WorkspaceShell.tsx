import * as Popover from "@radix-ui/react-popover";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, Outlet, useNavigate } from "@tanstack/react-router";
import { clsx } from "clsx";
import { Check, ChevronDown, DotsVertical, LayoutGrid, Logout, Moon, Plus, Server, Settings, ShieldLock, Sun } from "@/components/ui/icons";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Logo } from "@/components/ui/Logo";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Membership } from "@/lib/api";
import { useTheme } from "@/lib/theme";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";

function Avatar({ name, size = 24, round }: { name: string; size?: number; round?: boolean }) {
  return (
    <span
      aria-hidden
      style={{ width: size, height: size, fontSize: size * 0.45 }}
      className={clsx("inline-flex shrink-0 items-center justify-center bg-brand-soft font-semibold text-brand-text", round ? "rounded-full" : "rounded-md")}
    >
      {name.trim().charAt(0).toUpperCase() || "?"}
    </span>
  );
}

function NavItem({ to, icon, children, badge }: { to: string; icon: ReactNode; children: ReactNode; badge?: ReactNode }) {
  return (
    <Link
      to={to}
      className="flex h-9 items-center gap-2.5 rounded-md px-2.5 text-base text-muted-foreground hover:bg-accent hover:text-foreground [&>svg]:size-4"
      activeProps={{ className: "!bg-accent !text-foreground [&>svg]:text-brand-text" }}
    >
      {icon}
      {children}
      {badge}
    </Link>
  );
}

const menuItem = "flex h-9 w-full items-center gap-2.5 rounded-md px-2.5 text-sm text-foreground hover:bg-accent [&>svg]:size-4 [&>svg]:text-muted-foreground";

function NewWorkspaceDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange: (o: boolean) => void; onCreated: (w: Membership) => void }) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  return (
    <Modal open={open} onOpenChange={onOpenChange} title="New workspace" description="Workspaces hold projects and members. You'll be its admin.">
      <form
        className="flex flex-col gap-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            const w = await api.createWorkspace(name);
            await qc.invalidateQueries({ queryKey: ["me"] });
            onCreated({ id: w.id, name: w.name, role: "admin" });
            setName("");
            onOpenChange(false);
          } catch (err) {
            setError(err instanceof Error ? err.message : "Could not create the workspace.");
          } finally {
            setBusy(false);
          }
        }}
      >
        {error ? <Alert>{error}</Alert> : null}
        <Field label="Name">{(p) => <Input {...p} autoFocus required maxLength={64} value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button type="submit" variant="primary" loading={busy} disabled={!name.trim()}>Create workspace</Button>
        </div>
      </form>
    </Modal>
  );
}

export function WorkspaceShell() {
  const { data: me } = useMe();
  const [current, select] = useCurrentWorkspace(me);
  const { theme, toggle } = useTheme();
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [newWs, setNewWs] = useState(false);
  const [wsOpen, setWsOpen] = useState(false);
  const [acctOpen, setAcctOpen] = useState(false);

  const updates = useQuery({
    queryKey: ["instance-updates"],
    queryFn: api.updates,
    enabled: !!me?.user.isInstanceAdmin,
    staleTime: 5 * 60_000,
    retry: false,
  });
  const updateBadge = me?.user.isInstanceAdmin && updates.data?.updateAvailable ? <span className="text-caps ml-auto text-info">Update</span> : null;

  if (!me || !current) return null;

  const logout = async () => {
    await api.logout().catch(() => undefined);
    qc.clear();
    await navigate({ to: "/login" });
  };

  return (
    <div className="grid h-full grid-cols-[224px_1fr] grid-rows-[56px_1fr]">
      <header className="col-span-2 flex items-center justify-between px-4">
        <Link to="/projects" aria-label="Modula Deploy home"><Logo /></Link>
      </header>

      <aside className="flex min-h-0 flex-col px-2 pb-2">
        <Popover.Root open={wsOpen} onOpenChange={setWsOpen}>
          <Popover.Trigger className="flex w-full items-center gap-2.5 rounded-md px-2.5 py-2 text-left hover:bg-accent">
            <Avatar name={current.name} />
            <span className="min-w-0 flex-1">
              <span className="flex items-center gap-1 text-sm font-semibold"><span className="truncate">{current.name}</span><ChevronDown className="size-3.5 shrink-0 text-muted-foreground" /></span>
              <span className="text-caps text-info">{current.role}</span>
            </span>
          </Popover.Trigger>
          <Popover.Portal>
            <Popover.Content align="start" sideOffset={4} className="anim-pop z-30 w-64 rounded-md bg-card p-1 shadow-menu">
              <div className="text-caps px-2.5 py-1.5 text-muted-foreground">Workspaces</div>
              {me.workspaces.map((w) => (
                <button key={w.id} className={menuItem} onClick={() => { select(w.id); setWsOpen(false); }}>
                  <Avatar name={w.name} size={20} />
                  <span className="flex-1 truncate text-left">{w.name}</span>
                  {w.id === current.id ? <Check className="!text-brand-text" /> : null}
                </button>
              ))}
              <div className="my-1 h-px bg-border" />
              <button className={menuItem} onClick={() => { setWsOpen(false); setNewWs(true); }}>
                <Plus /> New workspace
              </button>
            </Popover.Content>
          </Popover.Portal>
        </Popover.Root>

        <div className="my-2 h-px bg-border" />
        <nav className="flex flex-col gap-0.5" aria-label="Workspace">
          <NavItem to="/projects" icon={<LayoutGrid />}>Projects</NavItem>
          <NavItem to="/settings" icon={<Settings />} badge={updateBadge}>Settings</NavItem>
        </nav>
        {me.user.isInstanceAdmin ? (
          <>
            <div className="my-2 h-px bg-border" />
            <nav className="flex flex-col gap-0.5" aria-label="Instance">
              <NavItem to="/servers" icon={<Server />}>Servers</NavItem>
            </nav>
          </>
        ) : null}
        <div className="my-2 h-px bg-border" />
        <nav className="flex flex-col gap-0.5" aria-label="Account">
          <NavItem to="/account/security" icon={<ShieldLock />}>Security</NavItem>
        </nav>

        <div className="mt-auto">
          <div className="my-2 h-px bg-border" />
          <Popover.Root open={acctOpen} onOpenChange={setAcctOpen}>
            <Popover.Trigger className="flex w-full items-center gap-2.5 rounded-md px-2.5 py-2 text-left hover:bg-accent" aria-label="Account menu">
              <Avatar name={me.user.name} round />
              <span className="min-w-0 flex-1 truncate text-sm">{me.user.name}</span>
              <DotsVertical className="size-4 text-muted-foreground" />
            </Popover.Trigger>
            <Popover.Portal>
              <Popover.Content side="top" align="start" sideOffset={6} className="anim-pop z-30 w-64 rounded-md bg-card p-1 shadow-menu">
                <div className="flex items-center gap-3 px-2.5 py-3">
                  <Avatar name={me.user.name} size={40} round />
                  <div className="min-w-0">
                    <div className="truncate text-sm font-semibold">{me.user.name}</div>
                    <div className="truncate text-xs text-muted-foreground">{me.user.email}</div>
                  </div>
                </div>
                <div className="my-1 h-px bg-border" />
                <Link to="/account/security" className={menuItem} onClick={() => setAcctOpen(false)}><ShieldLock /> Account security</Link>
                <div className="my-1 h-px bg-border" />
                <div className="flex items-center justify-between px-1">
                  <button className="flex h-9 items-center gap-2 rounded-md px-1.5 text-sm text-muted-foreground hover:text-foreground" onClick={toggle}>
                    {theme === "dark" ? <Sun className="size-4" /> : <Moon className="size-4" />}
                    {theme === "dark" ? "Light theme" : "Dark theme"}
                  </button>
                  <button className="flex h-9 items-center gap-2 rounded-md px-1.5 text-sm text-error hover:bg-destructive/10" onClick={() => void logout()}>
                    Log out <Logout className="size-4" />
                  </button>
                </div>
              </Popover.Content>
            </Popover.Portal>
          </Popover.Root>
        </div>
      </aside>

      <main className="mr-2 mb-2 min-h-0 overflow-auto rounded-md border border-border bg-card">
        <div className="mx-auto max-w-[880px] px-8 py-10">
          <Outlet />
        </div>
      </main>

      <NewWorkspaceDialog open={newWs} onOpenChange={setNewWs} onCreated={(w) => select(w.id)} />
    </div>
  );
}
