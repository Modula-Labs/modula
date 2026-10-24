import type { ReactNode } from "react";
import { Logo } from "./ui/Logo";

export function AuthCard({ title, subtitle, children, footer }: { title: string; subtitle?: ReactNode; children: ReactNode; footer?: ReactNode }) {
  return (
    <div className="dotgrid flex min-h-full flex-col items-center justify-center px-4 py-12">
      <div className="mb-8">
        <Logo />
      </div>
      {/* Kobra Card surface (radius xl, 1px foreground ring) with the dialog's 28px inset and title. */}
      <main className="anim-modal w-full max-w-[400px] rounded-xl bg-card p-7 text-card-foreground shadow-sm ring-1 ring-foreground/10">
        <h1 className="text-lg leading-6 font-bold tracking-[-0.45px]">{title}</h1>
        {subtitle ? <p className="mt-2 text-sm text-muted-foreground">{subtitle}</p> : null}
        <div className="mt-6">{children}</div>
      </main>
      {footer ? <div className="mt-6 text-sm text-muted-foreground">{footer}</div> : null}
    </div>
  );
}
