import { useState } from "react";
import { Spinner } from "@/components/ui/Button";
import { Kbd } from "@/components/ui/misc";

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/** Full-width SQL input. Cmd/Ctrl+Enter runs it. */
export function SqlBox({
  placeholder,
  running,
  disabled,
  onRun,
}: {
  placeholder: string;
  running: boolean;
  disabled?: boolean;
  onRun: (sql: string) => void;
}) {
  const [sql, setSql] = useState("");
  return (
    <div className="relative">
      <textarea
        value={sql}
        disabled={disabled}
        rows={sql.includes("\n") ? Math.min(8, sql.split("\n").length) : 1}
        spellCheck={false}
        aria-label="SQL"
        placeholder={placeholder}
        onChange={(e) => setSql(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            if (sql.trim() && !running) onRun(sql.trim());
          }
        }}
        className="block min-h-10 w-full resize-none rounded-lg border border-input bg-transparent py-2.5 pr-24 pl-3 font-mono text-[13px] leading-5 placeholder:text-muted-foreground outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-input/30"
      />
      <div className="pointer-events-none absolute top-2.5 right-3 flex items-center gap-1 text-xs text-muted-foreground">
        {running ? (
          <Spinner />
        ) : (
          <>
            <Kbd>{isMac ? "⌘" : "Ctrl"}</Kbd>
            <Kbd>↵</Kbd>
          </>
        )}
      </div>
    </div>
  );
}
