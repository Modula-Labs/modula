import { useEffect, useRef, useState } from "react";

export type NavItem = { id: string; label: string };

/** Space the nav needs to the left of the content column (width + gap). */
const NEEDED_GUTTER = 190;

/**
 * Docs-style "on this page" list that sits in the empty gutter left of the
 * content column. Render it inside a `relative` wrapper that is the content
 * column; it positions itself outside that wrapper's left edge.
 */
export function SectionNav({ items }: { items: NavItem[] }) {
  const ref = useRef<HTMLElement>(null);
  const [active, setActive] = useState(items[0]?.id ?? "");
  const [fits, setFits] = useState(false);
  const key = items.map((i) => i.id).join("|");

  // Only show when there is actually room in the gutter.
  useEffect(() => {
    const measure = () => {
      const wrapper = ref.current?.parentElement;
      if (!wrapper) return;
      const scroller = wrapper.closest("main") ?? document.body;
      const room =
        wrapper.getBoundingClientRect().left -
        scroller.getBoundingClientRect().left;
      setFits(room >= NEEDED_GUTTER);
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);

  // Scroll-spy.
  useEffect(() => {
    const els = key
      .split("|")
      .map((id) => document.getElementById(id))
      .filter((e): e is HTMLElement => !!e);
    if (!els.length) return;
    const visible = new Set<string>();
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) visible.add(e.target.id);
          else visible.delete(e.target.id);
        }
        const first = els.find((el) => visible.has(el.id));
        if (first) setActive(first.id);
      },
      { rootMargin: "-80px 0px -60% 0px" },
    );
    els.forEach((el) => io.observe(el));
    return () => io.disconnect();
  }, [key]);

  const go = (id: string) => (e: React.MouseEvent) => {
    e.preventDefault();
    const el = document.getElementById(id);
    if (!el) return;
    const reduce = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    el.scrollIntoView({ behavior: reduce ? "auto" : "smooth", block: "start" });
    history.replaceState(null, "", `#${id}`);
    setActive(id);
  };

  return (
    <nav
      ref={ref}
      aria-label="On this page"
      className={`absolute top-0 right-full mr-8 hidden h-full w-36 min-[1100px]:block ${fits ? "" : "min-[1100px]:invisible"}`}
    >
      <ul className="sticky top-4 flex flex-col border-l border-border">
        {items.map((it) => {
          const on = it.id === active;
          return (
            <li key={it.id} className="-ml-px">
              <a
                href={`#${it.id}`}
                onClick={go(it.id)}
                aria-current={on ? "location" : undefined}
                className={`block border-l py-1 pl-3 text-[13px] leading-5 transition-colors ${
                  on
                    ? "border-primary font-medium text-foreground"
                    : "border-transparent text-muted-foreground hover:text-foreground"
                }`}
              >
                {it.label}
              </a>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
