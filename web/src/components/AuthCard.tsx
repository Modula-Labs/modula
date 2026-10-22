import type { ReactNode } from "react";
import { Logo } from "./ui/Logo";

export function AuthCard({ title, subtitle, children, footer }: { title: string; subtitle?: ReactNode; children: ReactNode; footer?: ReactNode }) {
  return (
    <div className="dotgrid flex min-h-full flex-col items-center justify-center px-4 py-12">
      <div className="mb-8">
        <Logo />
      </div>
      <main className="anim-modal w-full max-w-[400px] rounded-lg border border-border bg-surface-2 p-8 shadow-modal">
        <h1 className="text-xl font-semibold">{title}</h1>
        {subtitle ? <p className="mt-1 text-sm text-fg-muted">{subtitle}</p> : null}
        <div className="mt-6">{children}</div>
      </main>
      {footer ? <div className="mt-6 text-sm text-fg-muted">{footer}</div> : null}
    </div>
  );
}
