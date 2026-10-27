/** The Modula "M" (Unbounded 900, outlined): white on a subtle near-black tile, in both themes. */
export function LogoMark({ size = 24 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 1024 1024" aria-hidden>
      <rect x="8" y="8" width="1008" height="1008" rx="224" fill="#111113" stroke="#ffffff" strokeOpacity="0.14" strokeWidth="16" />
      <path fill="#ffffff" transform="translate(512 512) scale(0.66) translate(-512 -512)" d="M907.2 272V752H739.54V334.08L771.7 337.92L599.25 752H424.75L252.3 339.84L284.46 335.36V752H116.8V272H389.85L543.14 663.04H482.23L634.15 272Z" />
    </svg>
  );
}

export function Logo() {
  return (
    <span className="inline-flex items-center gap-2 text-foreground" aria-label="Modula Deploy">
      <LogoMark />
      <span className="text-[15px] font-semibold tracking-[-0.02em]">
        modula <span className="font-normal text-muted-foreground">deploy</span>
      </span>
    </span>
  );
}
