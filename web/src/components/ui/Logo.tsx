/** 2×2 modules, top-right filled with the accent (docs/ui-spec.md §8.4). */
export function LogoMark({ size = 24 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden>
      <rect x="2.5" y="2.5" width="8.5" height="8.5" rx="2.5" fill="none" stroke="currentColor" strokeWidth="1.6" />
      <rect x="13" y="2.5" width="8.5" height="8.5" rx="2.5" className="fill-accent" />
      <rect x="2.5" y="13" width="8.5" height="8.5" rx="2.5" fill="none" stroke="currentColor" strokeWidth="1.6" />
      <rect x="13" y="13" width="8.5" height="8.5" rx="2.5" fill="none" stroke="currentColor" strokeWidth="1.6" />
    </svg>
  );
}

export function Logo() {
  return (
    <span className="inline-flex items-center gap-2 text-fg" aria-label="Modula Cloud">
      <LogoMark />
      <span className="text-[15px] font-semibold tracking-[-0.02em]">
        modula <span className="font-normal text-fg-muted">cloud</span>
      </span>
    </span>
  );
}
